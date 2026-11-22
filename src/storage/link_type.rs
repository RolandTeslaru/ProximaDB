//! Link type tables: per link type, "follows" and "followed by", each stored as three arrays.

pub use crate::core::link_type::Id;

use crate::storage::node_type;

/// One table ("follows" or "followed by") as three arrays:
/// - `starts`: where each node's part begins in the other two; one entry per node of every type, plus "end";
/// - `points_to`: the other node of each connection, all nodes' parts back to back;
/// - `links`: the link number of each connection, same positions as `points_to`.
pub struct Table {
    pub starts: Vec<u64>,
    pub points_to: Vec<u64>,
    pub links: Vec<u64>,
}

impl Table {
    /// Builds from (from node, to node, link) triples, grouping by `from` (a counting sort).
    pub fn build(node_count: usize, triples: &[(u64, u64, u64)]) -> Self {
        let mut starts = vec![0u64; node_count + 1];
        for &(from, _, _) in triples {
            starts[from as usize + 1] += 1;
        }
        for i in 0..node_count {
            starts[i + 1] += starts[i];
        }
        let mut next = starts.clone();
        let mut points_to = vec![0u64; triples.len()];
        let mut links = vec![0u64; triples.len()];
        for &(from, to, link) in triples {
            let at = next[from as usize] as usize;
            points_to[at] = to;
            links[at] = link;
            next[from as usize] += 1;
        }
        Self {
            starts,
            points_to,
            links,
        }
    }

    /// A node's part of `points_to`.
    #[inline]
    pub fn neighbours(&self, node: usize) -> &[u64] {
        let from = self.starts[node] as usize;
        let to = self.starts[node + 1] as usize;
        &self.points_to[from..to]
    }

    #[inline]
    pub fn degree(&self, node: usize) -> u64 {
        self.starts[node + 1] - self.starts[node]
    }

    pub fn len(&self) -> usize {
        self.points_to.len()
    }

    pub fn is_empty(&self) -> bool {
        self.points_to.is_empty()
    }

    pub fn bytes(&self) -> usize {
        (self.starts.capacity() + self.points_to.capacity() + self.links.capacity()) * 8
    }
}

/// One link type: which node types it connects, and its two tables.
pub struct Storage {
    pub name: String,
    pub from_type: node_type::Id,
    pub to_type: node_type::Id,
    /// "follows": row = from node.
    pub forward: Table,
    /// "followed by": row = to node.
    pub backward: Table,
}

impl Storage {
    pub fn build(
        name: &str,
        from_type: node_type::Id,
        to_type: node_type::Id,
        node_count: usize,
        triples: &[(u64, u64, u64)],
    ) -> Self {
        let forward = Table::build(node_count, triples);
        let reversed: Vec<(u64, u64, u64)> = triples.iter().map(|&(f, t, e)| (t, f, e)).collect();
        let backward = Table::build(node_count, &reversed);
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
