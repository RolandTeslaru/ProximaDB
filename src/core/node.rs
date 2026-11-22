//! Node numbers.

/// A node's number across all node types, in creation order. Used by link type arrays, `Bitset`s and
/// `node_type::Map`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GlobalId(pub u64);

/// A node's position within its own node type. Used by property arrays; means nothing without its
/// `node_type::Id`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id(pub u64);

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
