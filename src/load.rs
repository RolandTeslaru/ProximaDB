//! Building a graph from dataset files (CSV for now; the design reads the same files through DuckDB).

use std::path::Path;

use crate::collections::IndexVec;
use crate::core::link;
use crate::storage::graph::Graph;
use crate::storage::keys::{KeyIndex, PrimaryKeys};
use crate::storage::link_type;
use crate::storage::node_type;
use crate::storage::property::{self, Kind};

/// What the ontology says about a node type and where its objects come from.
pub struct NodeTypeSpec {
    pub name: String,
    pub file: String,
    pub primary_key: String,
    pub properties: Vec<(String, Kind)>,
}

/// A link type backed by a join dataset: a column per side holding that side's primary key.
pub struct LinkTypeSpec {
    pub name: String,
    pub from_type: String,
    pub to_type: String,
    pub file: String,
    pub from_column: String,
    pub to_column: String,
}

pub struct Schema {
    pub node_types: Vec<NodeTypeSpec>,
    pub link_types: Vec<LinkTypeSpec>,
}

pub fn load(dir: &Path, schema: &Schema) -> Result<Graph, Box<dyn std::error::Error>> {
    let mut map = node_type::Map::new(schema.node_types.len());
    let mut keys = PrimaryKeys::new();
    let mut key_index = KeyIndex::default();
    let mut node_types = IndexVec::new();

    // Nodes: one type after another, each appended in file order.
    for (ti, spec) in schema.node_types.iter().enumerate() {
        let t = node_type::Id(ti as u16);
        let mut reader = csv::Reader::from_path(dir.join(&spec.file))?;
        let headers = reader.headers()?.clone();
        let column = |name: &str| {
            headers
                .iter()
                .position(|h| h == name)
                .ok_or_else(|| format!("{}: no column {name}", spec.file))
        };
        let key_col = column(&spec.primary_key)?;
        let prop_cols: Vec<usize> = spec
            .properties
            .iter()
            .map(|(n, _)| column(n))
            .collect::<Result<_, _>>()?;
        let mut raw: Vec<Vec<Option<String>>> = vec![Vec::new(); spec.properties.len()];
        let mut record = csv::StringRecord::new();
        while reader.read_record(&mut record)? {
            let key = record.get(key_col).unwrap_or("").as_bytes();
            let (g, _) = map.push(t);
            keys.push(key);
            key_index.insert(t, key, g);
            for (p, &c) in prop_cols.iter().enumerate() {
                let v = record.get(c).unwrap_or("");
                raw[p].push(if v.is_empty() {
                    None
                } else {
                    Some(v.to_string())
                });
            }
        }
        let properties = spec
            .properties
            .iter()
            .zip(raw)
            .map(|((name, kind), values)| property::build(name, *kind, &values))
            .collect();
        node_types.push(node_type::Storage {
            name: spec.name.clone(),
            properties,
        });
    }

    let type_id = |name: &str| -> Result<node_type::Id, String> {
        schema
            .node_types
            .iter()
            .position(|t| t.name == name)
            .map(|i| node_type::Id(i as u16))
            .ok_or(format!("unknown node type {name}"))
    };

    // Link types: (from position, to position, link) triples, then the two tables.
    let mut link_types = IndexVec::new();
    for spec in &schema.link_types {
        let mut next_link = 0u32;
        let (ft, tt) = (type_id(&spec.from_type)?, type_id(&spec.to_type)?);
        let mut reader = csv::Reader::from_path(dir.join(&spec.file))?;
        let headers = reader.headers()?.clone();
        let fc = headers
            .iter()
            .position(|h| h == spec.from_column)
            .ok_or("missing from column")?;
        let tc = headers
            .iter()
            .position(|h| h == spec.to_column)
            .ok_or("missing to column")?;
        let mut triples = Vec::new();
        let mut record = csv::ByteRecord::new();
        while reader.read_byte_record(&mut record)? {
            let from = key_index
                .get(&keys, &map, ft, &record[fc])
                .ok_or("link to an unknown object")?;
            let to = key_index
                .get(&keys, &map, tt, &record[tc])
                .ok_or("link to an unknown object")?;
            triples.push((map.get(from).1, map.get(to).1, link::Id(next_link)));
            next_link += 1;
        }
        link_types.push(link_type::Storage::build(
            &spec.name,
            ft,
            tt,
            map.count(ft) as usize,
            map.count(tt) as usize,
            &triples,
        ));
    }

    let in_file_order = Graph {
        node_types,
        node_type_mappings: map,
        link_types,
        keys,
        key_index,
    };
    // Lay the nodes out with linked nodes close together: a rebuild with no edits renumbers them.
    Ok(crate::kinetic::rebuild::rebuild(&in_file_order, &[]).graph)
}
