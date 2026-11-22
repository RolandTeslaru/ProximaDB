//! Node numbers.

/// A node's number across all node types, in creation order. Used by walks, `Bitset`s over the whole graph, the
/// side store and `node_type::Map`.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GlobalId(pub u64);

/// A node's position within its own node type: at most 4,294,967,296 nodes per type. Used by link type tables and
/// property arrays; means nothing without its `node_type::Id`.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id(pub u32);

impl GlobalId {
    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

impl Id {
    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}
