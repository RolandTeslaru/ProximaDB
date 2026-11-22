//! A dataset read in batches of rows, column by column, the same way whatever its file format.
//!
//! The loader asks a batch for a column's keys (as bytes, without allocating) or for its values as cells of a property
//! kind. A CSV batch parses text; a Parquet batch reads typed Arrow arrays and parses only columns stored as text.

use std::fmt;
use std::path::Path;

use crate::core::Kind;
use crate::core::node::GlobalId;
use crate::storage::property::{self, Cell};

use super::{csv, parquet};

/// Rows per batch: large enough to amortise per-batch work, small enough to stay in cache.
pub(crate) const BATCH_ROWS: usize = 65_536;


#[derive(Debug)]
pub struct LoadError(pub String);


impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}


impl std::error::Error for LoadError {}


impl LoadError {
    pub(crate) fn at(path: &Path, message: impl fmt::Display) -> Self {
        Self(format!("{}: {message}", path.display()))
    }
}


/// A row's key, looked up among the nodes it should name.
pub(crate) enum Key {
    Found(GlobalId),
    /// No value: no link.
    Empty,
    /// A value that names no node.
    Dangling,
}


impl Key {
    pub(crate) fn of(
        key: Option<&[u8]>,
        find: &(dyn Fn(&[u8]) -> Option<GlobalId> + Sync),
    ) -> Self {
        match key {
            None => Key::Empty,
            Some(key) => find(key).map_or(Key::Dangling, Key::Found),
        }
    }
}


/// Lookups of many keys go to every core: the key index is read-only by then, and each lookup is a random read.
pub(crate) type Find<'f> = dyn Fn(&[u8]) -> Option<GlobalId> + Sync + 'f;


pub(crate) trait Batch: Sync {
    fn rows(&self) -> usize;

    /// Calls `each(row, key)` for every row: the column's value as key bytes, or `None` when it is empty.
    fn each_key(
        &self,
        column: &str,
        each: &mut dyn FnMut(usize, Option<&[u8]>),
    ) -> Result<(), LoadError>;

    /// Every row's key in `column`, looked up with `find`, in parallel.
    fn lookup_keys(&self, column: &str, find: &Find) -> Result<Vec<Key>, LoadError>;

    /// The column's values as cells of `kind`, one per row; `None` where a row has no value.
    fn cells(&self, column: &str, kind: Kind) -> Result<Vec<Option<Cell>>, LoadError>;
}


/// Reads `columns` of the dataset at `path` (a `.parquet` file, or else CSV) and calls `each` for every batch.
pub(crate) fn read(
    path: &Path,
    columns: &[&str],
    each: &mut dyn FnMut(&dyn Batch) -> Result<(), LoadError>,
) -> Result<(), LoadError> {
    let is_parquet = path
        .extension()
        .is_some_and(|extension| extension == "parquet");

    if is_parquet {
        parquet::read(path, columns, each)
    } else {
        csv::read(path, columns, each)
    }
}


/// A value stored as text, read as `kind`. Empty or unreadable text is no value.
pub(crate) fn cell_from_text(text: &str, kind: Kind) -> Option<Cell> {
    if text.is_empty() {
        return None;
    }

    match kind {
        Kind::String => Some(Cell::Str(text.to_string())),
        Kind::Integer | Kind::Long => text.parse().ok().map(Cell::Int),
        Kind::Double => text.parse().ok().map(Cell::F64),
        Kind::Date => property::parse_date(text).map(|days| Cell::Int(days as i64)),
        Kind::Timestamp => property::parse_timestamp(text).map(Cell::Int),

        Kind::Boolean => match text {
            "true" | "TRUE" | "True" | "1" => Some(Cell::Bool(true)),
            "false" | "FALSE" | "False" | "0" => Some(Cell::Bool(false)),
            _ => None,
        },
    }
}
