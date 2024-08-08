// src/hash.rs
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Hash {
    pub d: [u8; 16],
}