//! The editor: applies one action's edits to one generation and publishes them together.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use rustc_hash::FxHashMap;

use crate::collections::IndexVec;
use crate::core::Value;
use crate::core::link;
use crate::core::node;
use crate::storage::link_type;
use crate::storage::node_type;

use super::changes::{Change, encode, property_index};
use super::live::{BACKWARD, FORWARD, Generation, POSITION_BITS, POSITION_MASK};
use super::side::Entry;

/// Applies edits to one generation. Apply an action's edits after `begin(seq)`, then `commit()` publishes them all
/// at once. Only one editor per generation (`store::Writer` makes sure).
pub struct Editor {
    generation: Arc<Generation>,
    seq: u64,
    /// Per link type and side, the writer's record of each node's last side-store chunk.
    tails: IndexVec<link_type::Id, [FxHashMap<u64, u64>; 2]>,
    counts: IndexVec<node_type::Id, u64>,
    /// Per link type, the number the next added link gets.
    next_link: IndexVec<link_type::Id, u32>,
    nodes: u64,
}

impl Editor {
    pub fn new(generation: Arc<Generation>) -> Self {
        let (graph, live) = (&generation.graph, &generation.live);
        let view = live.view(graph);
        Self {
            seq: live.published(),
            tails: live.side.iter().map(|_| Default::default()).collect(),
            counts: graph.node_types.ids().map(|t| view.count(t)).collect(),
            next_link: graph
                .link_types
                .iter()
                .map(|r| r.forward.len() as u32)
                .collect(),
            nodes: view.nodes,
            generation,
        }
    }

    pub fn generation(&self) -> &Arc<Generation> {
        &self.generation
    }

    /// The action being applied (or the last one published).
    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn begin(&mut self, seq: u64) {
        assert!(
            seq > self.generation.live.published(),
            "sequence numbers must grow"
        );
        self.seq = seq;
    }

    pub fn commit(&mut self) {
        self.generation
            .live
            .nodes
            .store(self.nodes, Ordering::Release);
        self.generation
            .live
            .published
            .store(self.seq, Ordering::Release);
    }

    pub fn get(&self, g: node::GlobalId) -> (node_type::Id, node::Id) {
        if g.0 < self.generation.live.base_nodes {
            self.generation.graph.node_type_mappings.get(g)
        } else {
            let e = self
                .generation
                .live
                .new_map
                .get(g.0 - self.generation.live.base_nodes)
                .expect("node");
            (
                node_type::Id((e >> POSITION_BITS) as u16),
                node::Id((e & POSITION_MASK) as _),
            )
        }
    }

    pub fn add_node(&mut self, t: node_type::Id, key: &[u8]) -> node::GlobalId {
        let g = self.nodes;
        let local = self.counts[t];
        self.counts[t] += 1;
        self.generation
            .live
            .new_map
            .push(((t.0 as u64) << POSITION_BITS) | local);
        self.generation.live.new_by_type[t].push(self.seq);
        let mut keys = self.generation.live.new_keys.write().unwrap();
        keys.by_key
            .insert((t.0, key.to_vec()), (node::GlobalId(g), self.seq));
        keys.by_node.push(key.to_vec());
        self.nodes += 1;
        node::GlobalId(g)
    }

    /// Index of property `name` of type `t`.
    pub fn property_index(&self, t: node_type::Id, name: &str) -> usize {
        property_index(&self.generation.graph, t, name)
    }

    pub fn set_property(&mut self, g: node::GlobalId, pi: usize, value: &Value) {
        let (t, local) = self.get(g);
        let p = &self.generation.graph.node_types[t].properties[pi];
        let log = &self.generation.live.props[t][pi];
        let raw = encode(p, log, value);
        log.log.push(Change {
            local: local.0 as u64,
            value: raw,
            seq: self.seq,
        });
    }

    pub fn delete_node(&mut self, g: node::GlobalId) {
        self.generation.live.deleted.set(g.0);
        self.generation
            .live
            .any_deleted
            .store(true, Ordering::Release);
    }

    fn mark_changed(&self, g: node::GlobalId) {
        self.generation.live.changed.set(g.0);
        self.generation
            .live
            .any_changes
            .store(true, Ordering::Release);
    }

    pub fn add_link(
        &mut self,
        r: link_type::Id,
        from: node::GlobalId,
        to: node::GlobalId,
    ) -> link::Id {
        let rel = &self.generation.graph.link_types[r];
        assert!(
            self.get(from).0 == rel.from_type && self.get(to).0 == rel.to_type,
            "link {} joins {:?} to {:?} nodes",
            rel.name,
            rel.from_type,
            rel.to_type
        );
        let link = link::Id(self.next_link[r]);
        self.next_link[r] += 1;
        for (side, node, other) in [(FORWARD, from, to), (BACKWARD, to, from)] {
            self.mark_changed(node);
            let tail = self.tails[r][side].get(&node.0).copied();
            let mut t = tail;
            self.generation.live.side[r][side].push(
                node,
                Entry {
                    target: other,
                    link,
                    seq: self.seq,
                },
                &mut t,
            );
            if let Some(c) = t {
                self.tails[r][side].insert(node.0, c);
            }
        }
        link
    }

    /// Removes one link `from → to` of link type `r`; false if there is none.
    pub fn remove_link(
        &mut self,
        r: link_type::Id,
        from: node::GlobalId,
        to: node::GlobalId,
    ) -> bool {
        let graph = &self.generation.graph;
        let base_nodes = self.generation.live.base_nodes;
        let rel = &graph.link_types[r];
        if self.get(from).0 != rel.from_type || self.get(to).0 != rel.to_type {
            return false;
        }
        let mut found = false;
        for (side, base, node, other) in [
            (FORWARD, &rel.forward, from, to),
            (BACKWARD, &rel.backward, to, from),
        ] {
            let mut done = false;
            // Packed links join packed nodes only.
            if node.0 < base_nodes && other.0 < base_nodes {
                let row = graph.node_type_mappings.get(node).1;
                let target = graph.node_type_mappings.get(other).1;
                for p in base.range(row) {
                    if base.points_to[p] == target
                        && self.generation.live.removed[r][side].test_and_set(p)
                    {
                        done = true;
                        break;
                    }
                }
            }
            if !done {
                done = self.generation.live.side[r][side]
                    .remove(node, other, self.seq)
                    .is_some();
            }
            if done {
                self.mark_changed(node);
                found = true;
            }
        }
        found
    }
}
