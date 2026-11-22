//! What every layer shares: the id types, values, directions and property kinds.

pub mod direction;
pub mod link;
pub mod link_type;
pub mod node;
pub mod node_type;
pub mod property;
pub mod value;

pub use direction::Direction;
pub use property::Kind;
pub use value::Value;
