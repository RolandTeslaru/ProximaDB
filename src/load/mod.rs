//! Building a graph from dataset files: Parquet (read column by column with arrow-rs) or CSV.
//!
//!   1. Nodes, one node type after another: each dataset's primary key and property columns. A row without a key,
//!      or with a key its type already has, is skipped and counted.
//!   2. Links, once every node exists (a key may point at a type loaded later). Each link type's pairs come from its
//!      backing, as Quarry defines link types:
//!      - `Dataset`: a join dataset with one key column per side;
//!      - `ForeignKey`: a property on one side's object type holding the other side's key;
//!      - `Object`: an intermediary object type, loaded as nodes of its own, with a key column per side. It becomes
//!        two link types, `<name>.a` (from side → intermediary) and `<name>.b` (intermediary → to side), so a walk
//!        from side a to side b crosses it forward, and its properties can be filtered like any node's.
//!
//!      An empty key makes no link; a key that matches no node is a dangling link. Both are counted in the report.
//!   3. The nodes are laid out with linked nodes close together: a rebuild with no edits renumbers them.

mod csv;
mod parquet;
mod source;

use std::path::Path;

use crate::collections::IndexVec;
use crate::core::node::GlobalId;
use crate::core::{Kind, link, node};
use crate::kinetic::rebuild;
use crate::storage::graph::Graph;
use crate::storage::keys::{KeyIndex, PrimaryKeys};
use crate::storage::link_type;
use crate::storage::node_type;
use crate::storage::property::{self, Cell};

pub use source::LoadError;

use source::{Batch, Key};


/// What the ontology says about a node type and where its objects come from.
pub struct NodeTypeSpec {
    pub name: String,
    /// The dataset file, relative to the data directory: `.parquet`, or else CSV.
    pub file: String,
    pub primary_key: String,
    pub properties: Vec<(String, Kind)>,
}


/// A link type between two node types: side a (`from_type`) and side b (`to_type`), and where its pairs come from.
pub struct LinkTypeSpec {
    pub name: String,
    pub from_type: String,
    pub to_type: String,
    pub backing: Backing,
}


pub enum Backing {
    /// A join dataset, one column per side holding that side's primary key.
    Dataset {
        file: String,
        from_column: String,
        to_column: String,
    },

    /// A property on one side's object type that holds the other side's primary key.
    ForeignKey { side: Side, property: String },

    /// An intermediary object type with one property per side holding that side's primary key.
    Object {
        node_type: String,
        from_property: String,
        to_property: String,
    },
}


#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// Side a: the link type's `from_type`.
    From,
    /// Side b: the link type's `to_type`.
    To,
}


pub struct Schema {
    pub node_types: Vec<NodeTypeSpec>,
    pub link_types: Vec<LinkTypeSpec>,
}


/// What the load skipped, per node type and link type.
#[derive(Debug, Default)]
pub struct Report {
    pub node_types: Vec<NodeTypeReport>,
    pub link_types: Vec<LinkTypeReport>,
}


#[derive(Debug)]
pub struct NodeTypeReport {
    pub name: String,
    pub nodes: u64,
    /// Rows with no primary key.
    pub without_key: u64,
    /// Rows whose key an earlier row of the type already had.
    pub duplicate_keys: u64,
}


#[derive(Debug)]
pub struct LinkTypeReport {
    pub name: String,
    pub links: u64,
    /// Pairs with an empty key on either side: no link.
    pub empty: u64,
    /// Pairs with a key that matches no node.
    pub dangling: u64,
}


pub fn load(dir: &Path, schema: &Schema) -> Result<Graph, LoadError> {
    load_with_report(dir, schema).map(|(graph, _)| graph)
}


pub fn load_with_report(dir: &Path, schema: &Schema) -> Result<(Graph, Report), LoadError> {
    let mut report = Report::default();
    let nodes = load_nodes(dir, schema, &mut report)?;
    let link_types = load_links(dir, schema, &nodes, &mut report)?;

    let in_file_order = Graph {
        node_types: nodes.storage,
        node_type_mappings: nodes.map,
        link_types,
        keys: nodes.keys,
        key_index: nodes.key_index,
    };

    let laid_out = rebuild::rebuild(&in_file_order, &[]).graph;

    Ok((laid_out, report))
}


// ---------------------------------------------------------------- 1. nodes


struct Nodes {
    storage: IndexVec<node_type::Id, node_type::Storage>,
    map: node_type::Map,
    keys: PrimaryKeys,
    key_index: KeyIndex,
}


impl Nodes {
    fn type_named(&self, name: &str) -> Result<node_type::Id, LoadError> {
        self.storage
            .position(|storage| storage.name == name)
            .ok_or_else(|| LoadError(format!("unknown node type {name}")))
    }


    fn spec_named<'s>(
        &self,
        schema: &'s Schema,
        name: &str,
    ) -> Result<&'s NodeTypeSpec, LoadError> {
        schema
            .node_types
            .iter()
            .find(|spec| spec.name == name)
            .ok_or_else(|| LoadError(format!("unknown node type {name}")))
    }


    fn find(&self, node_type: node_type::Id, key: &[u8]) -> Option<GlobalId> {
        self.key_index.get(&self.keys, &self.map, node_type, key)
    }


    fn position(&self, node: GlobalId) -> node::Id {
        self.map.get(node).1
    }
}


fn load_nodes(dir: &Path, schema: &Schema, report: &mut Report) -> Result<Nodes, LoadError> {
    let mut nodes = Nodes {
        storage: IndexVec::new(),
        map: node_type::Map::new(schema.node_types.len()),
        keys: PrimaryKeys::new(),
        key_index: KeyIndex::default(),
    };

    for (index, spec) in schema.node_types.iter().enumerate() {
        let node_type = node_type::Id(index as u16);
        let (properties, type_report) = load_node_type(dir, spec, node_type, &mut nodes)?;

        nodes.storage.push(node_type::Storage {
            name: spec.name.clone(),
            primary_key: spec.primary_key.clone(),
            properties,
        });

        report.node_types.push(type_report);
    }

    Ok(nodes)
}


/// One node type's dataset: a node per row with a new key, and its property columns.
fn load_node_type(
    dir: &Path,
    spec: &NodeTypeSpec,
    node_type: node_type::Id,
    nodes: &mut Nodes,
) -> Result<(Vec<property::Property>, NodeTypeReport), LoadError> {
    let mut columns: Vec<&str> = vec![&spec.primary_key];
    columns.extend(spec.properties.iter().map(|(name, _)| name.as_str()));

    let mut cells: Vec<Vec<Option<Cell>>> = vec![Vec::new(); spec.properties.len()];
    let mut report = NodeTypeReport {
        name: spec.name.clone(),
        nodes: 0,
        without_key: 0,
        duplicate_keys: 0,
    };

    source::read(&dir.join(&spec.file), &columns, &mut |batch: &dyn Batch| {
        let mut kept = vec![false; batch.rows()];

        batch.each_key(&spec.primary_key, &mut |row, key| {
            let Some(key) = key else {
                report.without_key += 1;
                return;
            };

            if nodes.find(node_type, key).is_some() {
                report.duplicate_keys += 1;
                return;
            }

            let (node, _) = nodes.map.push(node_type);

            nodes.keys.push(key);
            nodes.key_index.insert(node_type, key, node);

            kept[row] = true;
            report.nodes += 1;
        })?;

        for ((name, kind), column) in spec.properties.iter().zip(&mut cells) {
            let values = batch.cells(name, *kind)?;

            column.extend(
                values
                    .into_iter()
                    .zip(&kept)
                    .filter(|(_, kept)| **kept)
                    .map(|(value, _)| value),
            );
        }

        Ok(())
    })?;

    let properties = spec
        .properties
        .iter()
        .zip(&cells)
        .map(|((name, kind), column)| property::build_cells(name, *kind, column))
        .collect();

    Ok((properties, report))
}


// ---------------------------------------------------------------- 2. links


/// One link type to build: its name, its two node types, and the dataset columns its pairs come from.
struct PairSource<'s> {
    name: String,
    from_type: node_type::Id,
    to_type: node_type::Id,
    file: &'s str,
    /// The column holding the from-side's key, and the node type that key belongs to (and likewise for to).
    from_column: &'s str,
    to_column: &'s str,
}


fn load_links(
    dir: &Path,
    schema: &Schema,
    nodes: &Nodes,
    report: &mut Report,
) -> Result<IndexVec<link_type::Id, link_type::Storage>, LoadError> {
    let mut storage = IndexVec::new();

    for spec in &schema.link_types {
        for source in pair_sources(schema, nodes, spec)? {
            let (link_type, link_report) = load_link_type(dir, nodes, &source)?;

            storage.push(link_type);
            report.link_types.push(link_report);
        }
    }

    Ok(storage)
}


/// Where a link type's pairs come from: one source, or two for an intermediary object type.
fn pair_sources<'s>(
    schema: &'s Schema,
    nodes: &Nodes,
    spec: &'s LinkTypeSpec,
) -> Result<Vec<PairSource<'s>>, LoadError> {
    let from_type = nodes.type_named(&spec.from_type)?;
    let to_type = nodes.type_named(&spec.to_type)?;

    match &spec.backing {
        Backing::Dataset {
            file,
            from_column,
            to_column,
        } => Ok(vec![PairSource {
            name: spec.name.clone(),
            from_type,
            to_type,
            file,
            from_column,
            to_column,
        }]),

        Backing::ForeignKey { side, property } => {
            let owner_name = match side {
                Side::From => &spec.from_type,
                Side::To => &spec.to_type,
            };
            let owner = nodes.spec_named(schema, owner_name)?;

            // The owner's own key on its side, the foreign key on the other.
            let (from_column, to_column) = match side {
                Side::From => (owner.primary_key.as_str(), property.as_str()),
                Side::To => (property.as_str(), owner.primary_key.as_str()),
            };

            Ok(vec![PairSource {
                name: spec.name.clone(),
                from_type,
                to_type,
                file: &owner.file,
                from_column,
                to_column,
            }])
        }

        Backing::Object {
            node_type,
            from_property,
            to_property,
        } => {
            let middle = nodes.type_named(node_type)?;
            let middle_spec = nodes.spec_named(schema, node_type)?;

            Ok(vec![
                PairSource {
                    name: format!("{}.a", spec.name),
                    from_type,
                    to_type: middle,
                    file: &middle_spec.file,
                    from_column: from_property,
                    to_column: &middle_spec.primary_key,
                },
                PairSource {
                    name: format!("{}.b", spec.name),
                    from_type: middle,
                    to_type,
                    file: &middle_spec.file,
                    from_column: &middle_spec.primary_key,
                    to_column: to_property,
                },
            ])
        }
    }
}


fn load_link_type(
    dir: &Path,
    nodes: &Nodes,
    source: &PairSource,
) -> Result<(link_type::Storage, LinkTypeReport), LoadError> {
    let mut triples = Vec::new();
    let mut report = LinkTypeReport {
        name: source.name.clone(),
        links: 0,
        empty: 0,
        dangling: 0,
    };

    let columns = [source.from_column, source.to_column];

    source::read(
        &dir.join(source.file),
        &columns,
        &mut |batch: &dyn Batch| {
            let from =
                batch.lookup_keys(source.from_column, &|key| nodes.find(source.from_type, key))?;
            let to = batch.lookup_keys(source.to_column, &|key| nodes.find(source.to_type, key))?;

            for (from, to) in from.into_iter().zip(to) {
                match (from, to) {
                    (Key::Found(from), Key::Found(to)) => {
                        let number = link::Id(triples.len() as u32);

                        triples.push((nodes.position(from), nodes.position(to), number));
                    }

                    (Key::Empty, _) | (_, Key::Empty) => report.empty += 1,

                    _ => report.dangling += 1,
                }
            }

            Ok(())
        },
    )?;

    report.links = triples.len() as u64;

    let storage = link_type::Storage::build(
        &source.name,
        source.from_type,
        source.to_type,
        nodes.map.count(source.from_type) as usize,
        nodes.map.count(source.to_type) as usize,
        &triples,
    );

    Ok((storage, report))
}
