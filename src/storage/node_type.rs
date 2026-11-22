//! Node types: each node's type and its position within that type.

pub use crate::core::node_type::Id;

use crate::collections::IndexVec;
use crate::core::{node, node_type};
use crate::storage::property::Property;

/// One node type's stored data: its name and its property columns.
pub struct Storage {
    pub name: String,
    pub properties: Vec<Property>,
}

/// Which node type each node is, and where each type's nodes start. Nodes are pushed one type after another, so each
/// type owns one contiguous range, `first(t) .. first(t) + count(t)`: a node's position within its type is its global
/// number minus the first. Its type is one 2-byte read.
#[derive(Default)]
pub struct Map {
    types: Vec<node_type::Id>,
    counts: IndexVec<node_type::Id, u64>,
    firsts: IndexVec<node_type::Id, node::GlobalId>,
}

impl Map {
    pub fn new(type_count: usize) -> Self {
        Self {
            types: Vec::new(),
            counts: IndexVec::filled(type_count, 0),
            firsts: IndexVec::filled(type_count, node::GlobalId(0)),
        }
    }

    pub fn reserve(&mut self, additional: usize) {
        self.types.reserve(additional);
    }

    /// Appends a node of type `t`; returns its global number and its position within `t`. Panics if `t`'s nodes
    /// would no longer be contiguous.
    pub fn push(&mut self, t: node_type::Id) -> (node::GlobalId, node::Id) {
        let local = self.counts[t];
        assert!(local <= u32::MAX as u64, "more nodes than a type can hold");
        let global = node::GlobalId(self.types.len() as u64);
        if local == 0 {
            self.firsts[t] = global;
        } else {
            assert_eq!(
                self.types.last(),
                Some(&t),
                "nodes of a type must be pushed together"
            );
        }
        self.counts[t] += 1;
        self.types.push(t);
        (global, node::Id(local as u32))
    }

    #[inline]
    pub fn type_of(&self, g: node::GlobalId) -> node_type::Id {
        self.types[g.index()]
    }

    #[inline]
    pub fn get(&self, g: node::GlobalId) -> (node_type::Id, node::Id) {
        let t = self.type_of(g);
        (t, self.position(t, g))
    }

    /// Position within `t` of global node `g`, which must be of type `t`.
    #[inline]
    pub fn position(&self, t: node_type::Id, g: node::GlobalId) -> node::Id {
        node::Id((g.0 - self.firsts[t].0) as u32)
    }

    /// The global number of `t`'s first node.
    #[inline]
    pub fn first(&self, t: node_type::Id) -> node::GlobalId {
        self.firsts[t]
    }

    pub fn len(&self) -> usize {
        self.types.len()
    }

    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
    }

    pub fn count(&self, t: node_type::Id) -> u64 {
        self.counts[t]
    }

    pub fn bytes(&self) -> usize {
        self.types.capacity() * size_of::<node_type::Id>() + self.counts.len() * 16
    }
}
