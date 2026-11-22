#![feature(portable_simd)]

//! ProximaDB: an in-memory index of Quarry's links for deep link questions. See IDEAS.md for the design.

pub mod collections;
pub mod core;
pub mod kinetic;
pub mod load;
pub mod query;
pub mod storage;

pub use collections::{AtomicBitset, Bitset};
pub use kinetic::{Generation, Live, Store, View, Writer};
pub use storage::Graph;
