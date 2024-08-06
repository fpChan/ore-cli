use std::{sync::Arc, time::Instant};
use std::sync::atomic::{AtomicU64, AtomicU32, Ordering};
use std::time::Duration;
use chrono::Local;
use colored::*;
use drillx::{
    equix::{self},
    Hash, Solution,
};
use ore_api::{
    consts::{BUS_ADDRESSES, BUS_COUNT, EPOCH_DURATION},
    state::{Config, Proof},
};
use rand::Rng;
use solana_program::pubkey::Pubkey;
use solana_rpc_client::spinner;
use solana_sdk::signer::Signer;

use crate::{
    args::MineArgs,
    send_and_confirm::ComputeBudget,
    utils::{amount_u64_to_string, get_clock, get_config, get_proof_with_authority, proof_pubkey},
    Miner,
};

const MIN: u32 = 20;

impl Miner {
    pub async fn mine(&self, args: MineArgs) {
        // Register, if needed.
        let signer = self.signer();
        self.open().await;

        // Check num threads
        self.check_num_cores(args.threads);

        // Start mining loop
        loop {
            // Fetch proof
            let proof = get_proof_with_authority(&self.rpc_client, signer.pubkey()).await;
            println!(
                "\n[{}] Stake balance: {} ORE",
                Local::now().format("%Y-%m-%d %H:%M:%S"),
                amount_u64_to_string(proof.balance)
            );

            // Run drillx
            let config = get_config(&self.rpc_client).await;
            if let Some(solution) = Self::find_hash_par(
                proof.clone(),
                args.threads,
                MIN, // min_difficulty
                Duration::from_secs(200), // 增加到 60 秒，以提高找到解的概率
            )
                .await
            {
                // Submit most difficult hash immediately
                let mut compute_budget = 500_000;
                let mut ixs = vec![ore_api::instruction::auth(proof_pubkey(signer.pubkey()))];
                if self.should_reset(config).await && rand::thread_rng().gen_range(0..100).eq(&0) {
                    compute_budget += 100_000;
                    ixs.push(ore_api::instruction::reset(signer.pubkey()));
                }
                ixs.push(ore_api::instruction::mine(
                    signer.pubkey(),
                    signer.pubkey(),
                    find_bus(),
                    solution,
                ));
                match self.send_and_confirm(&ixs, ComputeBudget::Fixed(compute_budget), false).await {
                    Ok(_) => println!("{}", "Successfully submitted mining solution.".green()),
                    Err(e) => println!("{} {}", "Failed to submit mining solution:".red(), e),
                }
            } else {
                println!("{}", "No solution found meeting minimum difficulty. Continuing to mine...".yellow());
            }

            // Small delay to prevent tight looping
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn find_hash_par(
        proof: Proof,
        threads: u64,
        min_difficulty: u32,
        duration: Duration,
    ) -> Option<Solution> {
        let start_time = Instant::now();
        let progress_bar = Arc::new(spinner::new_progress_bar());
        progress_bar.set_message("Mining...");

        let best_nonce = Arc::new(AtomicU64::new(0));
        let best_difficulty = Arc::new(AtomicU32::new(0));
        let best_hash = Arc::new(std::sync::RwLock::new(Hash::default()));

        let handles: Vec<_> = (0..threads)
            .map(|i| {
                std::thread::spawn({
                    let proof = proof.clone();
                    let progress_bar = progress_bar.clone();
                    let best_nonce = best_nonce.clone();
                    let best_difficulty = best_difficulty.clone();
                    let best_hash = best_hash.clone();
                    move || {
                        let mut memory = equix::SolverMemory::new();
                        let mut nonce = u64::MAX.saturating_div(threads).saturating_mul(i);

                        while start_time.elapsed() < duration {
                            if let Ok(hx) = drillx::hash_with_memory(
                                &mut memory,
                                &proof.challenge,
                                &nonce.to_le_bytes(),
                            ) {
                                let difficulty = hx.difficulty();
                                let current_best = best_difficulty.load(Ordering::Relaxed);
                                if difficulty > current_best {
                                    if best_difficulty.compare_exchange(current_best, difficulty, Ordering::SeqCst, Ordering::Relaxed).is_ok() {
                                        best_nonce.store(nonce, Ordering::Relaxed);
                                        let mut best_hash_guard = best_hash.write().unwrap();
                                        best_hash_guard.h.copy_from_slice(&hx.h);
                                        best_hash_guard.d = hx.d;
                                        println!("New best hash found: {} (difficulty: {})",
                                                 bs58::encode(hx.h).into_string(), difficulty);
                                    }
                                }
                            }

                            if nonce % 100 == 0 && start_time.elapsed() >= duration {
                                break;
                            }

                            nonce += 1;

                            if i == 0 && nonce % 1000 == 0 {
                                progress_bar.set_message(format!(
                                    "Mining... ({} sec remaining)",
                                    duration.as_secs().saturating_sub(start_time.elapsed().as_secs()),
                                ));
                            }
                        }
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        let final_best_nonce = best_nonce.load(Ordering::Relaxed);
        let final_best_difficulty = best_difficulty.load(Ordering::Relaxed);
        let final_best_hash = {
            let hash_guard = best_hash.read().unwrap();
            Hash { h: hash_guard.h, d: hash_guard.d }
        };

        progress_bar.finish_with_message(format!(
            "Best hash: {} (difficulty: {})",
            bs58::encode(final_best_hash.h).into_string(),
            final_best_difficulty
        ));

        if final_best_difficulty >= min_difficulty {
            Some(Solution::new(final_best_hash.d, final_best_nonce.to_le_bytes()))
        } else {
            None
        }
    }

    pub fn check_num_cores(&self, threads: u64) {
        // Check num threads
        let num_cores = num_cpus::get() as u64;
        if threads.gt(&num_cores) {
            println!(
                "{} Number of threads ({}) exceeds available cores ({})",
                "WARNING".bold().yellow(),
                threads,
                num_cores
            );
        }
    }

    async fn should_reset(&self, config: Config) -> bool {
        let clock = get_clock(&self.rpc_client).await;
        config
            .last_reset_at
            .saturating_add(EPOCH_DURATION)
            .saturating_sub(5) // Buffer
            .le(&clock.unix_timestamp)
    }

    async fn get_cutoff(&self, proof: Proof, buffer_time: u64) -> u64 {
        let clock = get_clock(&self.rpc_client).await;
        proof
            .last_hash_at
            .saturating_add(60)
            .saturating_sub(buffer_time as i64)
            .saturating_sub(clock.unix_timestamp)
            .max(0) as u64
    }
}

// TODO Pick a better strategy (avoid draining bus)
fn find_bus() -> Pubkey {
    let i = rand::thread_rng().gen_range(0..BUS_COUNT);
    BUS_ADDRESSES[i]
}
