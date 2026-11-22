//! Cypher, for the questions ProximaDB answers: one start node, optionally a link pattern and an end node, filters on
//! the end node and on every node stepped onto, and either the graph found or a count.
//!
//!   1. `lexer`: text to tokens.
//!   2. `parser`: tokens to a syntax tree (the grammar is at the top of `parser.rs`).
//!   3. `plan`: the tree checked against the graph (labels, link types, properties, values) and run as a walk:
//!      `subgraph::neighbourhood` for a graph, `walk::reach` for counts, `subgraph::shortest_path` for a path.
//!
//! Anything outside the subset is an error that says so, never a slow fallback.
//!
//! ```cypher
//! MATCH (a:User {nodeId: 42})-[:FOLLOWS*1..4 (r, n | n.status = 'active')]->(b:User)
//! WHERE b.spent >= 100
//! RETURN b LIMIT 500
//! ```

mod lexer;
mod parser;
mod plan;

use std::collections::HashMap;
use std::fmt;

use crate::core::Value;
use crate::kinetic::live::View;
use crate::query::subgraph::Subgraph;

/// At most this many nodes come back in a graph answer; the count stays exact.
pub const MAX_LIMIT: u64 = 10_000;

/// Without `LIMIT`, a graph answer has at most this many matches.
pub const DEFAULT_LIMIT: u64 = 1_000;

/// The deepest a variable-length link may go.
pub const MAX_DEPTH: u32 = 1_100;


#[derive(Debug)]
pub struct CypherError {
    pub message: String,
    /// Byte offset in the query text, for parse errors.
    pub offset: Option<usize>,
}


impl CypherError {
    pub(crate) fn at(offset: usize, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            offset: Some(offset),
        }
    }


    pub(crate) fn plain(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            offset: None,
        }
    }
}


impl fmt::Display for CypherError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.offset {
            Some(offset) => write!(f, "{} (at character {offset})", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}


/// What a query answers.
#[derive(Debug)]
pub struct Answer {
    /// The nodes and links to draw, when the query returns nodes or a path.
    pub graph: Option<Subgraph>,
    /// How many nodes match in all, when the query counts them or returns a graph (which may show fewer).
    pub count: Option<u64>,
    /// The length of the path, when the query asks for a shortest path.
    pub path_length: Option<u32>,
}


/// Parses, checks and runs `text` on `view`. `params` fill the `$name` values.
pub fn run(
    view: &View,
    text: &str,
    params: &HashMap<String, Value>,
) -> Result<Answer, CypherError> {
    let tokens = lexer::tokens(text)?;
    let query = parser::parse(tokens)?;

    plan::run(view, &query, params)
}
