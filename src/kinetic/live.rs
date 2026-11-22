//! Edits on top of the packed graph, and consistent views for searches.
//!
//! - New links go to the side store (per link type table, chunks with sequence numbers).
//! - Removed links: a bit per position of the packed "points to"; side-store entries get the removing sequence number.
//! - Deleted nodes: a bit per node. Walks never step onto a deleted node.
//! - New nodes: appended to block lists; their numbers continue after the packed ones.
//! - Property changes: a change log per property; the packed columns stay unchanged until the rebuild.
//!
//! One `Editor` (driven by `store::Writer`) applies edits in sequence order and publishes each action at once (`commit`). A `View` reads the
//! published sequence number when it starts and ignores everything newer, so a search never sees half an action.
//! Deleted bits and removed packed-link bits carry no sequence number: a search may see one that happened while it
//! ran.

use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rustc_hash::FxHashMap;

use crate::collections::IndexVec;
use crate::collections::bitset::AtomicBitset;
use crate::collections::blocks::{BlockList, GrowBitset};
use crate::core::Direction;
use crate::core::node;
use crate::kinetic::changes::PropertyLog;
use crate::kinetic::side::SideTable;
use crate::storage::graph::Graph;
use crate::storage::link_type::{self, Table};
use crate::storage::node_type;

/// Index of the "follows" table of a link type's pair.
pub(crate) const FORWARD: usize = 0;
/// Index of the "followed by" table.
pub(crate) const BACKWARD: usize = 1;

pub(crate) const POSITION_BITS: u32 = 48;
pub(crate) const POSITION_MASK: u64 = (1 << POSITION_BITS) - 1;

#[derive(Default)]
pub(crate) struct NewKeys {
    pub(crate) by_key: FxHashMap<(u16, Vec<u8>), (u64, u64)>,
    pub(crate) by_node: Vec<Vec<u8>>,
}

pub struct Live {
    pub(crate) published: AtomicU64,
    pub(crate) nodes: AtomicU64,
    pub(crate) any_changes: AtomicBool,
    pub(crate) any_deleted: AtomicBool,
    pub(crate) base_nodes: u64,
    /// Per link type, `[FORWARD]` for "follows" and `[BACKWARD]` for "followed by".
    pub(crate) side: IndexVec<link_type::Id, [SideTable; 2]>,
    pub(crate) removed: IndexVec<link_type::Id, [AtomicBitset; 2]>,
    pub(crate) changed: GrowBitset,
    pub(crate) deleted: GrowBitset,
    pub(crate) new_map: BlockList<u64>,
    pub(crate) new_by_type: IndexVec<node_type::Id, BlockList<u64>>,
    pub(crate) new_keys: RwLock<NewKeys>,
    pub(crate) props: IndexVec<node_type::Id, Vec<PropertyLog>>,
}

/// A packed graph and the edits made on it since it was built.
pub struct Generation {
    pub graph: Graph,
    pub live: Live,
}

impl Generation {
    pub fn new(graph: Graph) -> Self {
        let live = Live::new(&graph);
        Self { graph, live }
    }

    /// A consistent view as of the last published action.
    pub fn view(&self) -> View<'_> {
        self.live.view(&self.graph)
    }
}

impl Live {
    pub fn new(graph: &Graph) -> Self {
        Self::starting_at(graph, 0)
    }

    /// No edits yet, with everything up to action `seq` already in the packed graph.
    pub fn starting_at(graph: &Graph, seq: u64) -> Self {
        let side = graph
            .link_types
            .iter()
            .map(|_| [SideTable::new(), SideTable::new()])
            .collect();
        let removed = graph
            .link_types
            .iter()
            .map(|r| {
                [
                    AtomicBitset::new(r.forward.len()),
                    AtomicBitset::new(r.backward.len()),
                ]
            })
            .collect();
        Self {
            published: AtomicU64::new(seq),
            nodes: AtomicU64::new(graph.node_count() as u64),
            any_changes: AtomicBool::new(false),
            any_deleted: AtomicBool::new(false),
            base_nodes: graph.node_count() as u64,
            side,
            removed,
            changed: GrowBitset::new(),
            deleted: GrowBitset::new(),
            new_map: BlockList::new(),
            new_by_type: graph.node_types.iter().map(|_| BlockList::new()).collect(),
            new_keys: RwLock::new(NewKeys::default()),
            props: graph
                .node_types
                .iter()
                .map(|t| {
                    t.properties
                        .iter()
                        .map(|_| PropertyLog {
                            log: BlockList::new(),
                            extra_strings: RwLock::new(Vec::new()),
                        })
                        .collect()
                })
                .collect(),
        }
    }

    /// A consistent view as of the last published action.
    pub fn view<'a>(&'a self, graph: &'a Graph) -> View<'a> {
        let upto = self.published.load(Ordering::Acquire);
        View {
            graph,
            live: self,
            upto,
            nodes: self.nodes.load(Ordering::Acquire),
            any_changes: self.any_changes.load(Ordering::Acquire),
            any_deleted: self.any_deleted.load(Ordering::Acquire),
        }
    }

    pub fn published(&self) -> u64 {
        self.published.load(Ordering::Acquire)
    }

    pub fn bytes(&self) -> usize {
        self.side
            .iter()
            .flatten()
            .map(SideTable::bytes)
            .sum::<usize>()
            + self
                .removed
                .iter()
                .flatten()
                .map(|b| b.word_count() * 8)
                .sum::<usize>()
            + self.changed.bytes()
            + self.deleted.bytes()
            + self.new_map.bytes()
            + self
                .props
                .iter()
                .flatten()
                .map(|p| p.log.bytes())
                .sum::<usize>()
    }

    pub fn side_chunks(&self) -> u64 {
        self.side.iter().flatten().map(SideTable::chunks).sum()
    }
}

/// One link type table as a walk reads it: packed part, side store, and removed bits.
pub struct TableRef<'a> {
    pub base: &'a Table,
    pub side: &'a SideTable,
    pub removed: &'a AtomicBitset,
}

/// What a search sees: everything published up to `upto`.
pub struct View<'a> {
    pub graph: &'a Graph,
    pub live: &'a Live,
    pub upto: u64,
    pub nodes: u64,
    pub(crate) any_changes: bool,
    pub(crate) any_deleted: bool,
}

impl<'a> View<'a> {
    pub fn node_count(&self) -> usize {
        self.nodes as usize
    }

    #[inline]
    pub fn get(&self, g: u64) -> (node_type::Id, node::Id) {
        if g < self.live.base_nodes {
            self.graph.nodes.get(node::GlobalId(g))
        } else {
            let e = self
                .live
                .new_map
                .get(g - self.live.base_nodes)
                .expect("node");
            (
                node_type::Id((e >> POSITION_BITS) as u16),
                node::Id(e & POSITION_MASK),
            )
        }
    }

    #[inline]
    pub fn type_of(&self, g: u64) -> node_type::Id {
        if g < self.live.base_nodes {
            self.graph.nodes.type_of(node::GlobalId(g))
        } else {
            self.get(g).0
        }
    }

    #[inline]
    pub fn is_deleted(&self, g: u64) -> bool {
        self.any_deleted && self.live.deleted.get(g)
    }

    /// For each node type, the tables a walk reads from a node of that type.
    pub fn tables_by_type(
        &self,
        link_types: &[link_type::Id],
        direction: Direction,
    ) -> IndexVec<node_type::Id, Vec<TableRef<'a>>> {
        let mut out = IndexVec::from_fn(self.graph.node_types.len(), |_| Vec::new());
        for &r in link_types {
            let rel = &self.graph.link_types[r];
            if matches!(direction, Direction::Forward | Direction::Both) {
                out[rel.from_type].push(TableRef {
                    base: &rel.forward,
                    side: &self.live.side[r][FORWARD],
                    removed: &self.live.removed[r][FORWARD],
                });
            }
            if matches!(direction, Direction::Backward | Direction::Both) {
                out[rel.to_type].push(TableRef {
                    base: &rel.backward,
                    side: &self.live.side[r][BACKWARD],
                    removed: &self.live.removed[r][BACKWARD],
                });
            }
        }
        out
    }

    #[inline]
    fn changed(&self, u: u64) -> bool {
        self.any_changes && self.live.changed.get(u)
    }

    /// Calls `f(v)` for every node `u` connects to in this table, as of this view.
    #[inline]
    pub fn for_each(&self, t: &TableRef, u: u64, mut f: impl FnMut(u64)) {
        let changed = self.changed(u);
        if u < self.live.base_nodes {
            let from = t.base.starts[u as usize] as usize;
            let to = t.base.starts[u as usize + 1] as usize;
            if !changed {
                for &v in &t.base.points_to[from..to] {
                    f(v);
                }
            } else {
                for p in from..to {
                    if !t.removed.get(p) {
                        f(t.base.points_to[p]);
                    }
                }
            }
        }
        if changed {
            t.side.for_each(u, self.upto, |v, _| f(v));
        }
    }

    /// Whether any node `u` connects to in this table satisfies `test`; stops at the first in the packed part.
    #[inline]
    pub fn any(
        &self,
        t: &TableRef,
        u: u64,
        mut test: impl FnMut(u64) -> bool,
        checked: &mut u64,
    ) -> bool {
        let changed = self.changed(u);
        if u < self.live.base_nodes {
            let from = t.base.starts[u as usize] as usize;
            let to = t.base.starts[u as usize + 1] as usize;
            for p in from..to {
                *checked += 1;
                if (!changed || !t.removed.get(p)) && test(t.base.points_to[p]) {
                    return true;
                }
            }
        }
        if changed {
            let mut found = false;
            t.side.for_each(u, self.upto, |v, _| {
                *checked += 1;
                if !found && test(v) {
                    found = true;
                }
            });
            return found;
        }
        false
    }

    #[inline]
    pub fn degree(&self, t: &TableRef, u: u64) -> u64 {
        if u < self.live.base_nodes {
            t.base.degree(u as usize)
        } else {
            0
        }
    }

    /// (type, key) → node, including nodes added since the rebuild.
    pub fn lookup(&self, t: node_type::Id, key: &[u8]) -> Option<u64> {
        if let Some(g) = self
            .graph
            .key_index
            .get(&self.graph.keys, &self.graph.nodes, t, key)
        {
            return (!self.is_deleted(g.0)).then_some(g.0);
        }
        let keys = self.live.new_keys.read().unwrap();
        keys.by_key
            .get(&(t.0, key.to_vec()))
            .filter(|&&(_, seq)| seq <= self.upto)
            .map(|&(g, _)| g)
            .filter(|&g| !self.is_deleted(g))
    }

    /// A node's primary key.
    pub fn key(&self, g: u64) -> Vec<u8> {
        if g < self.live.base_nodes {
            self.graph.keys.get(node::GlobalId(g)).to_vec()
        } else {
            self.live.new_keys.read().unwrap().by_node[(g - self.live.base_nodes) as usize].clone()
        }
    }

    /// How many nodes of type `t` this view sees (packed ones plus those added up to `upto`).
    pub fn count(&self, t: node_type::Id) -> u64 {
        let base = self.graph.nodes.count(t);
        let added = &self.live.new_by_type[t];
        let mut n = added.len();
        while n > 0 && added.get(n - 1).unwrap() > self.upto {
            n -= 1;
        }
        base + n
    }
}
