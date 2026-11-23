//! The editor: applies one action's edits to one generation and publishes them together.
//!
//! An action is applied as `begin(seq)`, its edits, then `commit()`, which publishes them all at once. Only one editor
//! works on a generation at a time (`store::Writer` makes sure).

use std::sync::Arc;

use rustc_hash::FxHashMap;

use crate::collections::IndexVec;
use crate::collections::bitset::AtomicBitset;
use crate::core::Value;
use crate::core::link;
use crate::core::node;
use crate::storage::link_type::{self, Table};
use crate::storage::node_type;

use super::changes::{Change, encode, property_index};
use super::draft::{BACKWARD, Draft, FORWARD, Generation};
use super::side::Entry;


pub struct Editor {
    generation: Arc<Generation>,
    seq: u64,
    /// Per link type and side, the writer's record of each node's last side-store chunk.
    tails: IndexVec<link_type::Id, [FxHashMap<u64, u64>; 2]>,
    /// Per node type, how many nodes it has: the position the next added one gets.
    counts: IndexVec<node_type::Id, u64>,
    /// Per link type, the number the next added link gets.
    next_link: IndexVec<link_type::Id, u32>,
    /// Nodes in the generation, published and not: the number the next added node gets.
    nodes: u64,
}


impl Editor {
    pub fn new(generation: Arc<Generation>) -> Self {
        let graph = &generation.graph;
        let view = generation.view();

        let tails = graph
            .link_types
            .iter()
            .map(|_| Default::default())
            .collect();
        let counts = graph.node_types.ids().map(|t| view.count(t)).collect();

        let next_link = graph
            .link_types
            .iter()
            .map(|storage| storage.forward.len() as u32)
            .collect();

        let seq = generation.draft.published();
        let nodes = view.nodes;

        Self {
            seq,
            tails,
            counts,
            next_link,
            nodes,
            generation,
        }
    }


    pub fn generation(&self) -> &Arc<Generation> {
        &self.generation
    }


    fn draft(&self) -> &Draft {
        &self.generation.draft
    }


    /// The action being applied (or the last one published).
    pub fn seq(&self) -> u64 {
        self.seq
    }


    pub fn begin(&mut self, seq: u64) {
        assert!(seq > self.draft().published(), "sequence numbers must grow");

        self.seq = seq;
    }


    /// Publishes `seq` with no edits of its own, if it is ahead of the last published action.
    pub fn publish_at_least(&mut self, seq: u64) {
        if seq > self.draft().published() {
            self.seq = seq;
            self.draft().publish(seq);
        }
    }


    pub fn commit(&mut self) {
        self.draft().publish(self.seq);
    }


    pub fn get(&self, g: node::GlobalId) -> (node_type::Id, node::Id) {
        self.draft().node(&self.generation.graph, g)
    }


    pub fn add_node(&mut self, node_type: node_type::Id, key: &[u8]) -> node::GlobalId {
        let node = node::GlobalId(self.nodes);
        let position = self.counts[node_type];

        self.draft()
            .add_node(node, node_type, position, key, self.seq);

        self.counts[node_type] += 1;
        self.nodes += 1;

        node
    }


    /// Index of property `name` of `node_type`.
    pub fn property_index(&self, node_type: node_type::Id, name: &str) -> usize {
        property_index(&self.generation.graph, node_type, name)
    }


    pub fn set_property(&mut self, node: node::GlobalId, property: usize, value: &Value) {
        let (node_type, position) = self.get(node);
        let column = &self.generation.graph.node_types[node_type].properties[property];
        let log = self.draft().property_log(node_type, property);

        let change = Change {
            local: position.0 as u64,
            value: encode(column, log, value),
            seq: self.seq,
        };

        self.draft().log_change(node_type, property, change);
    }


    pub fn delete_node(&mut self, node: node::GlobalId) {
        self.draft().delete(node);
    }


    /// Adds a link `from → to` of `link_type`, in both directions; returns its number within the link type.
    pub fn add_link(
        &mut self,
        link_type: link_type::Id,
        from: node::GlobalId,
        to: node::GlobalId,
    ) -> link::Id {
        let storage = &self.generation.graph.link_types[link_type];

        assert!(
            self.get(from).0 == storage.from_type && self.get(to).0 == storage.to_type,
            "link {} joins {:?} to {:?} nodes",
            storage.name,
            storage.from_type,
            storage.to_type
        );

        let link = link::Id(self.next_link[link_type]);
        self.next_link[link_type] += 1;

        for (side, node, other) in [(FORWARD, from, to), (BACKWARD, to, from)] {
            self.draft().mark_changed(node);

            let entry = Entry {
                target: other,
                link,
                seq: self.seq,
            };

            let mut tail = self.tails[link_type][side].get(&node.0).copied();

            self.draft()
                .side(link_type, side)
                .push(node, entry, &mut tail);

            if let Some(chunk) = tail {
                self.tails[link_type][side].insert(node.0, chunk);
            }
        }

        link
    }


    /// Removes one link `from → to` of `link_type`, in both directions; false if there is none.
    pub fn remove_link(
        &mut self,
        link_type: link_type::Id,
        from: node::GlobalId,
        to: node::GlobalId,
    ) -> bool {
        let storage = &self.generation.graph.link_types[link_type];

        if self.get(from).0 != storage.from_type || self.get(to).0 != storage.to_type {
            return false;
        }

        let mut found = false;

        for (side, table, node, other) in [
            (FORWARD, &storage.forward, from, to),
            (BACKWARD, &storage.backward, to, from),
        ] {
            let removed_bits = self.draft().removed(link_type, side);
            let side_table = self.draft().side(link_type, side);

            let removed = self.remove_packed(table, removed_bits, node, other)
                || side_table.remove(node, other, self.seq).is_some();

            if removed {
                self.draft().mark_changed(node);
                found = true;
            }
        }

        found
    }


    /// Marks the first packed link `node → other` in `table` as removed. Packed links join packed nodes only.
    fn remove_packed(
        &self,
        table: &Table,
        removed: &AtomicBitset,
        node: node::GlobalId,
        other: node::GlobalId,
    ) -> bool {
        let base_nodes = self.draft().base_nodes();

        if node.0 >= base_nodes || other.0 >= base_nodes {
            return false;
        }

        let map = &self.generation.graph.node_type_mappings;
        let row = map.get(node).1;
        let target = map.get(other).1;

        table
            .range(row)
            .any(|position| table.points_to[position] == target && removed.test_and_set(position))
    }
}
