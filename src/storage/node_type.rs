//! Node types: each node's type and its position within that type.

pub use crate::core::node_type::Id;

use crate::collections::IndexVec;
use crate::core::node;
use crate::storage::property::Property;

/// One node type's stored data: its name and its property columns.
pub struct Storage {
    pub name: String,
    pub properties: Vec<Property>,
}

const POSITION_BITS: u32 = 48;
const POSITION_MASK: u64 = (1 << POSITION_BITS) - 1;

/// One 64-bit entry per node: its node type in the top 16 bits, its position within that type in the low 48.
/// One memory read gives both.
#[derive(Default)]
pub struct Map {
    entries: Vec<u64>,
    counts: IndexVec<Id, u64>,
}

impl Map {
    pub fn new(type_count: usize) -> Self {
        Self {
            entries: Vec::new(),
            counts: IndexVec::filled(type_count, 0),
        }
    }

    pub fn reserve(&mut self, additional: usize) {
        self.entries.reserve(additional);
    }

    /// Appends a node of type `t`; returns its global number and its position within `t`.
    pub fn push(&mut self, t: Id) -> (node::GlobalId, node::Id) {
        let local = self.counts[t];
        assert!(local <= POSITION_MASK);
        self.counts[t] += 1;
        let global = node::GlobalId(self.entries.len() as u64);
        self.entries.push(((t.0 as u64) << POSITION_BITS) | local);
        (global, node::Id(local))
    }

    #[inline]
    pub fn get(&self, g: node::GlobalId) -> (Id, node::Id) {
        let entry = self.entries[g.index()];
        (
            Id((entry >> POSITION_BITS) as u16),
            node::Id(entry & POSITION_MASK),
        )
    }

    #[inline]
    pub fn type_of(&self, g: node::GlobalId) -> Id {
        Id((self.entries[g.index()] >> POSITION_BITS) as u16)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn count(&self, t: Id) -> u64 {
        self.counts[t]
    }

    pub fn type_count(&self) -> usize {
        self.counts.len()
    }

    pub fn bytes(&self) -> usize {
        self.entries.capacity() * 8
    }
}
