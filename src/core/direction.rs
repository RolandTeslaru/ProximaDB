//! Which way a walk follows connections.

/// Which way a walk follows connections.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Backward,
    Both,
}
