//! The packed graph: built at load or rebuild, never changed afterwards.

pub mod graph;
pub mod keys;
pub mod link_type;
pub mod node_type;
pub mod property;

pub use graph::Graph;
