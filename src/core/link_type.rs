//! Link type numbers.

use crate::collections::Idx;

/// A link type number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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
