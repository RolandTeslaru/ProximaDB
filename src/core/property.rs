//! Property kinds.

/// Quarry's property types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    String,
    Integer,
    Long,
    Double,
    Boolean,
    Date,
    Timestamp,
}
