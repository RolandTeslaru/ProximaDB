//! Link type tables: per link type, "follows" and "followed by", each stored as three arrays.

pub use crate::core::link_type::Id;

use crate::core::{link, node};
use crate::storage::node_type;

/// One table ("follows" or "followed by") as three arrays, all in positions within node types (`node::Id`):
/// - `starts`: where each node's part begins in the other two; one entry per node of the table's own type
///   (`from_type` for "follows", `to_type` for "followed by"), plus "end";
/// - `points_to`: the position of the other node of each connection within its type, all parts back to back;
/// - `links`: the link number of each connection, same positions as `points_to`.
pub struct Table {
    pub starts: Vec<link::Position>,
    pub points_to: Vec<node::Id>,
    pub links: Vec<link::Id>,
}

impl Table {
    /// Builds from (from position, to position, link) triples, grouping by `from` (a counting sort). `rows` is
    /// the number of nodes of the table's own type.
    pub fn build(rows: usize, triples: &[(node::Id, node::Id, link::Id)]) -> Self {
        assert!(
            triples.len() <= u32::MAX as usize,
            "more links than a table can hold"
        );
        let mut starts = vec![link::Position(0); rows + 1];
        for &(from, _, _) in triples {
            starts[from.index() + 1].0 += 1;
        }
        for i in 0..rows {
            starts[i + 1].0 += starts[i].0;
        }
        let mut next = starts.clone();
        let mut points_to = vec![node::Id(0); triples.len()];
        let mut links = vec![link::Id(0); triples.len()];
        for &(from, to, link) in triples {
            let at = next[from.index()].index();
            points_to[at] = to;
            links[at] = link;
            next[from.index()].0 += 1;
        }
        Self {
            starts,
            points_to,
            links,
        }
    }

    /// The range of `points_to` holding a node's part.
    #[inline]
    pub fn range(&self, node: node::Id) -> std::ops::Range<usize> {
        self.starts[node.index()].index()..self.starts[node.index() + 1].index()
    }

    /// A node's part of `points_to`.
    #[inline]
    pub fn neighbours(&self, node: node::Id) -> &[node::Id] {
        &self.points_to[self.range(node)]
    }

    #[inline]
    pub fn degree(&self, node: node::Id) -> u64 {
        (self.starts[node.index() + 1].0 - self.starts[node.index()].0) as u64
    }

    /// Number of nodes the table has a row for.
    pub fn rows(&self) -> usize {
        self.starts.len() - 1
    }

    pub fn len(&self) -> usize {
        self.points_to.len()
    }

    pub fn is_empty(&self) -> bool {
        self.points_to.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.starts.capacity() * size_of::<link::Position>()
            + self.points_to.capacity() * size_of::<node::Id>()
            + self.links.capacity() * size_of::<link::Id>()
    }
}

/// One link type: which node types it connects, and its two tables.
pub struct Storage {
    pub name: String,
    pub from_type: node_type::Id,
    pub to_type: node_type::Id,
    /// "follows": a row per `from_type` node, pointing to `to_type` positions.
    pub forward: Table,
    /// "followed by": a row per `to_type` node, pointing to `from_type` positions.
    pub backward: Table,
}

impl Storage {
    /// From (from position, to position, link) triples; `from_count` and `to_count` are the two types' node counts.
    pub fn build(
        name: &str,
        from_type: node_type::Id,
        to_type: node_type::Id,
        from_count: usize,
        to_count: usize,
        triples: &[(node::Id, node::Id, link::Id)],
    ) -> Self {
        let forward = Table::build(from_count, triples);
        let reversed: Vec<_> = triples.iter().map(|&(f, t, e)| (t, f, e)).collect();
        let backward = Table::build(to_count, &reversed);
        Self {
            name: name.to_string(),
            from_type,
            to_type,
            forward,
            backward,
        }
    }

    pub fn bytes(&self) -> usize {
        self.forward.bytes() + self.backward.bytes()
    }
}
