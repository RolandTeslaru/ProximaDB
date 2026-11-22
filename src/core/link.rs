//! Link numbers.

/// A link's number within its link type, in creation order (renumbered by a rebuild). Unique together with its
/// `link_type::Id`. At most 4,294,967,296 links per link type.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id(pub u32);

/// A position in a link type table's `points_to` (and `links`): where a node's part starts.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Position(pub u32);

impl Position {
    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}
