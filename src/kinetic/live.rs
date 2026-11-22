//! Edits on top of the packed graph, and consistent views for searches.
//!
//! - New links go to the side store (per link type table, chunks with sequence numbers).
//! - Removed links: a bit per position of the packed "points to"; side-store entries get the removing sequence
//!   number.
//! - Deleted nodes: a bit per node. Walks never step onto a deleted node.
//! - New nodes: appended to block lists; their numbers continue after the packed ones.
//! - Property changes: a change log per property; the packed columns stay unchanged until the rebuild.
//!
//! One `Editor` (driven by `store::Writer`) applies edits in sequence order through the `pub(super)` methods below,
//! and publishes each action at once. A `View` reads the published sequence number when it starts and ignores
//! everything newer, so a search never sees half an action. Deleted bits and removed packed-link bits carry no
//! sequence number: a search may see one that happened while it ran.

use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rustc_hash::FxHashMap;

use crate::collections::IndexVec;
use crate::collections::bitset::AtomicBitset;
use crate::collections::blocks::{BlockList, GrowBitset};
use crate::core::Direction;
use crate::core::node;
use crate::kinetic::changes::{Change, PropertyLog};
use crate::kinetic::side::SideTable;
use crate::storage::graph::Graph;
use crate::storage::link_type::{self, Table};
use crate::storage::node_type;

/// Index of the "follows" table of a link type's pair.
pub(crate) const FORWARD: usize = 0;

/// Index of the "followed by" table.
pub(crate) const BACKWARD: usize = 1;

/// A new node's entry: its node type above these bits, its position within the type below.
const POSITION_BITS: u32 = 32;
const POSITION_MASK: u64 = (1 << POSITION_BITS) - 1;


/// Keys of the nodes added since the rebuild.
#[derive(Default)]
struct NewKeys {
    /// (node type, key) → (node, the action that added it).
    by_key: FxHashMap<(u16, Vec<u8>), (node::GlobalId, u64)>,
    /// Key of each added node, in the order they were added.
    by_node: Vec<Vec<u8>>,
}


pub struct Live {
    published: AtomicU64,
    nodes: AtomicU64,
    any_changes: AtomicBool,
    any_deleted: AtomicBool,
    /// Nodes in the packed graph; added nodes are numbered from here.
    base_nodes: u64,
    /// Per link type, `[FORWARD]` for "follows" and `[BACKWARD]` for "followed by".
    side: IndexVec<link_type::Id, [SideTable; 2]>,
    removed: IndexVec<link_type::Id, [AtomicBitset; 2]>,
    changed: GrowBitset,
    deleted: GrowBitset,
    new_nodes: BlockList<u64>,
    /// Per node type, the action that added each of its new nodes, in order.
    new_by_type: IndexVec<node_type::Id, BlockList<u64>>,
    new_keys: RwLock<NewKeys>,
    property_logs: IndexVec<node_type::Id, Vec<PropertyLog>>,
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
            .map(|storage| {
                [
                    AtomicBitset::new(storage.forward.len()),
                    AtomicBitset::new(storage.backward.len()),
                ]
            })
            .collect();

        let property_logs = graph
            .node_types
            .iter()
            .map(|node_type| {
                node_type
                    .properties
                    .iter()
                    .map(|_| PropertyLog::new())
                    .collect()
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
            new_nodes: BlockList::new(),
            new_by_type: graph.node_types.iter().map(|_| BlockList::new()).collect(),
            new_keys: RwLock::new(NewKeys::default()),
            property_logs,
        }
    }


    /// A consistent view as of the last published action.
    pub fn view<'a>(&'a self, graph: &'a Graph) -> View<'a> {
        View {
            graph,
            live: self,
            upto: self.published.load(Ordering::Acquire),
            nodes: self.nodes.load(Ordering::Acquire),
            any_changes: self.any_changes.load(Ordering::Acquire),
            any_deleted: self.any_deleted.load(Ordering::Acquire),
        }
    }


    pub fn published(&self) -> u64 {
        self.published.load(Ordering::Acquire)
    }


    pub fn bytes(&self) -> usize {
        let side: usize = self.side.iter().flatten().map(SideTable::bytes).sum();

        let removed: usize = self
            .removed
            .iter()
            .flatten()
            .map(|bits| bits.word_count() * 8)
            .sum();

        let logs: usize = self
            .property_logs
            .iter()
            .flatten()
            .map(|log| log.log.bytes())
            .sum();

        side + removed + self.changed.bytes() + self.deleted.bytes() + self.new_nodes.bytes() + logs
    }


    pub fn side_chunks(&self) -> u64 {
        self.side.iter().flatten().map(SideTable::chunks).sum()
    }


    /// A node's type and its position within that type, packed or added since the rebuild.
    #[inline]
    pub(crate) fn node(&self, graph: &Graph, g: node::GlobalId) -> (node_type::Id, node::Id) {
        if g.0 < self.base_nodes {
            return graph.node_type_mappings.get(g);
        }

        let entry = self
            .new_nodes
            .get(g.0 - self.base_nodes)
            .expect("every node below the published count has an entry");

        (
            node_type::Id((entry >> POSITION_BITS) as u16),
            node::Id((entry & POSITION_MASK) as u32),
        )
    }


    pub(crate) fn property_log(&self, node_type: node_type::Id, property: usize) -> &PropertyLog {
        &self.property_logs[node_type][property]
    }


    pub(super) fn base_nodes(&self) -> u64 {
        self.base_nodes
    }


    pub(super) fn side(&self, link_type: link_type::Id, side: usize) -> &SideTable {
        &self.side[link_type][side]
    }


    pub(super) fn removed(&self, link_type: link_type::Id, side: usize) -> &AtomicBitset {
        &self.removed[link_type][side]
    }


    // ---------------------------------------------------------------- the editor's writes


    /// Makes everything written since the last publish visible to new views at once.
    pub(super) fn publish(&self, nodes: u64, seq: u64) {
        self.nodes.store(nodes, Ordering::Release);
        self.published.store(seq, Ordering::Release);
    }


    pub(super) fn add_node(
        &self,
        node: node::GlobalId,
        node_type: node_type::Id,
        position: u64,
        key: &[u8],
        seq: u64,
    ) {
        self.new_nodes
            .push(((node_type.0 as u64) << POSITION_BITS) | position);
        self.new_by_type[node_type].push(seq);

        let mut keys = self
            .new_keys
            .write()
            .expect("no writer panics holding the lock");

        keys.by_key.insert((node_type.0, key.to_vec()), (node, seq));
        keys.by_node.push(key.to_vec());
    }


    pub(super) fn log_change(&self, node_type: node_type::Id, property: usize, change: Change) {
        self.property_logs[node_type][property].log.push(change);
    }


    pub(super) fn delete(&self, node: node::GlobalId) {
        self.deleted.set(node.0);
        self.any_deleted.store(true, Ordering::Release);
    }


    /// Marks a node whose links changed, so walks read its removed bits and side-store entries.
    pub(super) fn mark_changed(&self, node: node::GlobalId) {
        self.changed.set(node.0);
        self.any_changes.store(true, Ordering::Release);
    }
}


/// One link type table as a walk reads it: packed part, side store, and removed bits. The packed part is in
/// positions within node types; `row_first` and `other_first` turn them back into global numbers.
pub struct TableRef<'a> {
    pub base: &'a Table,
    pub side: &'a SideTable,
    pub removed: &'a AtomicBitset,
    /// Global number of the first node of the type the rows are for.
    pub row_first: u64,
    /// Global number of the first node of the type `points_to` holds.
    pub other_first: u64,
    /// The type of every node this table leads to (packed and side store alike).
    pub other_type: node_type::Id,
}


impl TableRef<'_> {
    /// The row of packed node `u` (a node of the table's row type).
    #[inline]
    fn row(&self, u: node::GlobalId) -> node::Id {
        node::Id((u.0 - self.row_first) as u32)
    }


    #[inline]
    fn global(&self, v: node::Id) -> node::GlobalId {
        node::GlobalId(self.other_first + v.0 as u64)
    }
}


/// What a search sees: everything published up to `upto`.
pub struct View<'a> {
    pub graph: &'a Graph,
    pub live: &'a Live,
    pub upto: u64,
    pub nodes: u64,
    any_changes: bool,
    any_deleted: bool,
}


impl<'a> View<'a> {
    pub fn node_count(&self) -> usize {
        self.nodes as usize
    }


    #[inline]
    pub fn get(&self, g: node::GlobalId) -> (node_type::Id, node::Id) {
        self.live.node(self.graph, g)
    }


    /// Position within `node_type` of node `g`, which must be of that type. No type lookup for packed nodes.
    #[inline]
    pub fn position(&self, node_type: node_type::Id, g: node::GlobalId) -> node::Id {
        if g.0 < self.live.base_nodes {
            return self.graph.node_type_mappings.position(node_type, g);
        }

        self.get(g).1
    }


    #[inline]
    pub fn type_of(&self, g: node::GlobalId) -> node_type::Id {
        if g.0 < self.live.base_nodes {
            return self.graph.node_type_mappings.type_of(g);
        }

        self.get(g).0
    }


    #[inline]
    pub fn is_deleted(&self, g: node::GlobalId) -> bool {
        self.any_deleted && self.live.deleted.get(g.0)
    }


    /// For each node type, the tables a walk reads from a node of that type.
    pub fn tables_by_type(
        &self,
        link_types: &[link_type::Id],
        direction: Direction,
    ) -> IndexVec<node_type::Id, Vec<TableRef<'a>>> {
        let map = &self.graph.node_type_mappings;
        let mut tables = IndexVec::from_fn(self.graph.node_types.len(), |_| Vec::new());

        for &link_type in link_types {
            let storage = &self.graph.link_types[link_type];
            let from_first = map.first(storage.from_type).0;
            let to_first = map.first(storage.to_type).0;

            if matches!(direction, Direction::Forward | Direction::Both) {
                tables[storage.from_type].push(TableRef {
                    base: &storage.forward,
                    side: self.live.side(link_type, FORWARD),
                    removed: self.live.removed(link_type, FORWARD),
                    row_first: from_first,
                    other_first: to_first,
                    other_type: storage.to_type,
                });
            }

            if matches!(direction, Direction::Backward | Direction::Both) {
                tables[storage.to_type].push(TableRef {
                    base: &storage.backward,
                    side: self.live.side(link_type, BACKWARD),
                    removed: self.live.removed(link_type, BACKWARD),
                    row_first: to_first,
                    other_first: from_first,
                    other_type: storage.from_type,
                });
            }
        }

        tables
    }


    #[inline]
    fn changed(&self, u: node::GlobalId) -> bool {
        self.any_changes && self.live.changed.get(u.0)
    }


    /// Calls `f(v)` for every node `u` connects to in this table, as of this view.
    #[inline]
    pub fn for_each(&self, table: &TableRef, u: node::GlobalId, mut f: impl FnMut(node::GlobalId)) {
        let changed = self.changed(u);

        if u.0 < self.live.base_nodes {
            let range = table.base.range(table.row(u));

            if changed {
                for position in range {
                    if !table.removed.get(position) {
                        f(table.global(table.base.points_to[position]));
                    }
                }
            } else {
                for &v in &table.base.points_to[range] {
                    f(table.global(v));
                }
            }
        }

        if changed {
            table.side.for_each(u, self.upto, |v, _| f(v));
        }
    }


    /// Whether any node `u` connects to in this table satisfies `test`; stops at the first in the packed part.
    #[inline]
    pub fn any(
        &self,
        table: &TableRef,
        u: node::GlobalId,
        mut test: impl FnMut(node::GlobalId) -> bool,
        checked: &mut u64,
    ) -> bool {
        let changed = self.changed(u);

        if u.0 < self.live.base_nodes {
            for position in table.base.range(table.row(u)) {
                *checked += 1;

                let present = !changed || !table.removed.get(position);

                if present && test(table.global(table.base.points_to[position])) {
                    return true;
                }
            }
        }

        if !changed {
            return false;
        }

        let mut found = false;

        table.side.for_each(u, self.upto, |v, _| {
            *checked += 1;

            if !found && test(v) {
                found = true;
            }
        });

        found
    }


    /// Links of `u` in the packed part of this table (added links aren't counted).
    #[inline]
    pub fn degree(&self, table: &TableRef, u: node::GlobalId) -> u64 {
        if u.0 < self.live.base_nodes {
            return table.base.degree(table.row(u));
        }

        0
    }


    /// (node type, key) → node, including nodes added since the rebuild.
    pub fn lookup(&self, node_type: node_type::Id, key: &[u8]) -> Option<node::GlobalId> {
        let graph = self.graph;
        let packed = graph
            .key_index
            .get(&graph.keys, &graph.node_type_mappings, node_type, key);

        if let Some(g) = packed {
            return (!self.is_deleted(g)).then_some(g);
        }

        let keys = self
            .live
            .new_keys
            .read()
            .expect("no writer panics holding the lock");

        keys.by_key
            .get(&(node_type.0, key.to_vec()))
            .filter(|&&(_, added_by)| added_by <= self.upto)
            .map(|&(g, _)| g)
            .filter(|&g| !self.is_deleted(g))
    }


    /// A node's primary key.
    pub fn key(&self, g: node::GlobalId) -> Vec<u8> {
        if g.0 < self.live.base_nodes {
            return self.graph.keys.get(g).to_vec();
        }

        let keys = self
            .live
            .new_keys
            .read()
            .expect("no writer panics holding the lock");

        keys.by_node[(g.0 - self.live.base_nodes) as usize].clone()
    }


    /// How many nodes of `node_type` this view sees: packed ones plus those added up to `upto`.
    pub fn count(&self, node_type: node_type::Id) -> u64 {
        let packed = self.graph.node_type_mappings.count(node_type);
        let added = &self.live.new_by_type[node_type];

        // Added nodes are in action order: drop the ones published after this view.
        let mut visible = added.len();

        while visible > 0 && added.get(visible - 1).expect("below len") > self.upto {
            visible -= 1;
        }

        packed + visible
    }
}
