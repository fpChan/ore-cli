use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use std::sync::atomic::Ordering;

use chrono::Local;
use colored::*;
use drillx::{
    equix::{self},
    Solution,
};
use ore_api::{
    consts::{BUS_ADDRESSES, BUS_COUNT, EPOCH_DURATION},
    state::{Config, Proof},
};
use rand::Rng;
use solana_program::native_token::lamports_to_sol;
use solana_program::pubkey::Pubkey;
use solana_rpc_client::spinner;
use solana_sdk::signer::Signer;

use crate::{
    args::MineArgs,
    Miner,
    send_and_confirm::ComputeBudget,
    utils::{amount_u64_to_string, get_clock, get_config, get_proof_with_authority, proof_pubkey},
};

mod hash;
const MIN: u32 = 22;
const CHUNK_SIZE: usize = 1_000_000;
const TIME_LIMIT: Duration = Duration::from_secs(150); // 2 minutes

impl Miner {
    pub async fn mine(&self, args: MineArgs) {
        // Register, if needed.
        let signer = self.signer();
        self.open().await;

        // Check num threads
        self.check_num_cores(args.threads);

        // Start mining loop
        let mut last_ore_bal = 0;
        let mut last_sol_bal = 0;
        loop {
            // Fetch proof
            let proof = get_proof_with_authority(&self.rpc_client, signer.pubkey()).await;
            let signer = self.signer();
            let client = self.rpc_client.clone();

            // Return error, if balance is zero
            let sol_bal = client.get_balance(&signer.pubkey()).await.unwrap();
            println!(
                "\n[{}] {} {} sol. stake balance: {} ore. last cost {} sol earn {} ore",
                Local::now().format("%Y-%m-%d %H:%M:%S"),
                &signer.pubkey(),
                lamports_to_sol(sol_bal),
                amount_u64_to_string(proof.balance),
                lamports_to_sol(last_sol_bal - sol_bal),
                amount_u64_to_string(proof.balance - last_ore_bal)
            );
            if sol_bal != last_sol_bal {
                last_sol_bal = sol_bal
            }
            if proof.balance != last_ore_bal {
                last_ore_bal = proof.balance;
            }

            // Run drillx
            let config = get_config(&self.rpc_client).await;
            if let Some(solution) = self.find_hash_par(
                proof.clone(),
                args.threads,
                MIN, // min_difficulty
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
                let priority_fee = self.priority_fee.load(Ordering::Relaxed);
                match self.send_and_confirm(&ixs, ComputeBudget::Fixed(compute_budget), false).await {
                    Ok(_) => println!("{} (priority_fee: {})", "Successfully submitted mining solution.".green(), priority_fee),
                    Err(e) => println!("{} {} (priority_fee: {})", "Failed to submit mining solution:".red(), e, priority_fee),
                }
            } else {
                println!("{}", "No solution found meeting minimum difficulty. Continuing to mine...".yellow());
            }

            // Small delay to prevent tight looping
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn find_hash_par(
        &self,
        proof: Proof,
        threads: u64,
        min_difficulty: u32,
    ) -> Option<Solution> {
        let progress_bar = Arc::new(spinner::new_progress_bar());
        progress_bar.set_message("Mining...");

        let best_results = Arc::new(Mutex::new(Vec::new()));
        let start_time = Instant::now();

        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let proof = proof.clone();
                let progress_bar = progress_bar.clone();
                let best_results = best_results.clone();

                std::thread::spawn(move || {
                    let mut memory = equix::SolverMemory::new();
                    let mut rng = rand::thread_rng();

                    let mut best_difficulty = 0;
                    let mut best_nonce = 0;
                    let mut best_hash = Arc::new(hash::Hash::default());

                    while Instant::now() - start_time < TIME_LIMIT {
                        let nonce_base = rng.gen::<u64>();

                        for (index, nonce) in (nonce_base..nonce_base + CHUNK_SIZE as u64).enumerate() {
                            if let Ok(hx) = drillx::hash_with_memory(
                                &mut memory,
                                &proof.challenge,
                                &nonce.to_le_bytes(),
                            ) {
                                let difficulty = hx.difficulty();
                                if difficulty > best_difficulty {
                                    best_difficulty = difficulty;
                                    best_nonce = nonce;
                                    best_hash = Arc::new(hash::Hash { d: hx.d });

                                    if difficulty >= min_difficulty {
                                        // Lock the best_results and update the vector
                                        let mut best_results_guard = best_results.lock().unwrap();
                                        best_results_guard.push(BestResult {
                                            difficulty,
                                            nonce,
                                            hash: best_hash.clone(),
                                        });
                                        break;
                                    }
                                }
                            }

                            if index % 2000 == 0 {
                                let elapsed = Instant::now() - start_time;
                                progress_bar.set_message(format!(
                                    "Mining... (index {} nonce: {}, difficulty: {}, time elapsed: {:.2}s)",
                                    index,
                                    nonce,
                                    best_difficulty,
                                    elapsed.as_secs_f64()
                                ));
                            }
                        }
                    }

                    // Lock the best_results and update the vector
                    let mut best_results_guard = best_results.lock().unwrap();
                    best_results_guard.push(BestResult {
                        difficulty: best_difficulty,
                        nonce: best_nonce,
                        hash: best_hash.clone(),
                    });
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        // Find the overall best result from the vector
        let mut best_results_guard = best_results.lock().unwrap();
        best_results_guard.sort_by(|a, b| b.difficulty.cmp(&a.difficulty));

        if let Some(best_result) = best_results_guard.first() {
            self.update_priority_fee(best_result.difficulty);
            Some(Solution::new(best_result.hash.d, best_result.nonce.to_le_bytes()))
            // if best_result.difficulty >= min_difficulty {
            //     self.update_priority_fee(best_result.difficulty);
            //     Some(Solution::new(best_result.hash.d, best_result.nonce.to_le_bytes()))
            // } else {
            //     None
            // }
        } else {
            None
        }
    }

    fn update_priority_fee(&self, difficulty: u32) {
        let new_fee = if difficulty <= 17 {
            10000
        } else if difficulty < 21 {
            30000
        } else if difficulty < 26 {
            150000
        } else if difficulty < 30 {
            500000
        } else if difficulty < 32 {
            700000
        } else {
            900000
        };
        self.priority_fee.store(new_fee, Ordering::Relaxed);
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

struct BestResult {
    difficulty: u32,
    nonce: u64,
    hash: Arc<hash::Hash>,
}