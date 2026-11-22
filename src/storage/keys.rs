//! Quarry primary keys ↔ node numbers.

use std::hash::{Hash, Hasher};

use rustc_hash::{FxHashMap, FxHasher};

use crate::core::node;
use crate::storage::node_type;

/// Node number → primary key: every key back to back in one byte array, plus where each starts.
#[derive(Default)]
pub struct PrimaryKeys {
    bytes: Vec<u8>,
    starts: Vec<u64>,
}

impl PrimaryKeys {
    pub fn new() -> Self {
        Self {
            bytes: Vec::new(),
            starts: vec![0],
        }
    }

    pub fn push(&mut self, key: &[u8]) {
        self.bytes.extend_from_slice(key);
        self.starts.push(self.bytes.len() as u64);
    }

    pub fn get(&self, g: node::GlobalId) -> &[u8] {
        &self.bytes[self.starts[g.index()] as usize..self.starts[g.index() + 1] as usize]
    }

    pub fn bytes(&self) -> usize {
        self.bytes.capacity() + self.starts.capacity() * 8
    }
}

/// (node type, primary key) → node number, keyed by a 64-bit hash. A lookup compares the stored key to rule out a
/// collision; the rare colliding keys go to a small second map.
#[derive(Default)]
pub struct KeyIndex {
    by_hash: FxHashMap<u64, u64>,
    collisions: FxHashMap<(u16, Vec<u8>), u64>,
}

fn hash(t: node_type::Id, key: &[u8]) -> u64 {
    let mut h = FxHasher::default();
    t.0.hash(&mut h);
    key.hash(&mut h);
    h.finish()
}

impl KeyIndex {
    pub fn with_capacity(n: usize) -> Self {
        Self {
            by_hash: FxHashMap::with_capacity_and_hasher(n, Default::default()),
            collisions: FxHashMap::default(),
        }
    }

    pub fn insert(&mut self, t: node_type::Id, key: &[u8], g: node::GlobalId) {
        let h = hash(t, key);
        if self.by_hash.contains_key(&h) {
            self.collisions.insert((t.0, key.to_vec()), g.0);
        } else {
            self.by_hash.insert(h, g.0);
        }
    }

    pub fn get(
        &self,
        keys: &PrimaryKeys,
        map: &node_type::Map,
        t: node_type::Id,
        key: &[u8],
    ) -> Option<node::GlobalId> {
        if let Some(&g) = self.by_hash.get(&hash(t, key)) {
            let g = node::GlobalId(g);
            if map.type_of(g) == t && keys.get(g) == key {
                return Some(g);
            }
        }
        self.collisions
            .get(&(t.0, key.to_vec()))
            .map(|&g| node::GlobalId(g))
    }

    pub fn bytes(&self) -> usize {
        // hashbrown: one control byte plus the (u64, u64) entry per bucket.
        self.by_hash.capacity() * (16 + 1) + self.collisions.len() * 64
    }
}
