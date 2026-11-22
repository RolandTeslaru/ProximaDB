//! Node type numbers.

use crate::collections::Idx;

/// A node type's number (up to 65,536 types).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id(pub u16);

impl Id {
    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

impl Idx for Id {
    #[inline]
    fn from_index(i: usize) -> Self {
        Id(i as u16)
    }

    #[inline]
    fn index(self) -> usize {
        self.0 as usize
    }
}
