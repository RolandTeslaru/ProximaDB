//! The whole stored graph.

use crate::collections::IndexVec;
use crate::core::Direction;
use crate::storage::keys::{KeyIndex, PrimaryKeys};
use crate::storage::link_type::{self, Table};
use crate::storage::node_type;
use crate::storage::property::Property;

pub struct Graph {
    pub node_types: IndexVec<node_type::Id, node_type::Storage>,
    pub nodes: node_type::Map,
    pub link_types: IndexVec<link_type::Id, link_type::Storage>,
    pub keys: PrimaryKeys,
    pub key_index: KeyIndex,
}

impl Graph {
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn node_type(&self, name: &str) -> Option<node_type::Id> {
        self.node_types.position(|t| t.name == name)
    }

    pub fn link_type(&self, name: &str) -> Option<link_type::Id> {
        self.link_types.position(|r| r.name == name)
    }

    pub fn property(&self, t: node_type::Id, name: &str) -> Option<&Property> {
        self.node_types[t]
            .properties
            .iter()
            .find(|p| p.name == name)
    }

    /// For each node type, the tables a walk reads from a node of that type: the link types that start (forward) or
    /// end (backward) at that type. Types a link type doesn't touch never read its table.
    pub fn tables_by_type(
        &self,
        link_types: &[link_type::Id],
        direction: Direction,
    ) -> IndexVec<node_type::Id, Vec<&Table>> {
        let mut out = IndexVec::from_fn(self.node_types.len(), |_| Vec::new());
        for &r in link_types {
            let rel = &self.link_types[r];
            if matches!(direction, Direction::Forward | Direction::Both) {
                out[rel.from_type].push(&rel.forward);
            }
            if matches!(direction, Direction::Backward | Direction::Both) {
                out[rel.to_type].push(&rel.backward);
            }
        }
        out
    }

    pub fn all_link_types(&self) -> Vec<link_type::Id> {
        self.link_types.ids().collect()
    }

    /// Memory held by each part, in bytes.
    pub fn memory(&self) -> Vec<(String, usize)> {
        let mut out = vec![
            ("node_type::Map".to_string(), self.nodes.bytes()),
            ("primary keys".to_string(), self.keys.bytes()),
            ("key index".to_string(), self.key_index.bytes()),
        ];
        for r in &self.link_types {
            out.push((format!("link type {} (both tables)", r.name), r.bytes()));
        }
        for t in &self.node_types {
            let bytes: usize = t.properties.iter().map(Property::bytes).sum();
            out.push((format!("properties of {}", t.name), bytes));
        }
        out
    }
}
