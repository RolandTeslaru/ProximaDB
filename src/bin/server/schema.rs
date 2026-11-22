//! The schema file: node types with their properties and datasets, link types with where their pairs come from.
//!
//! A link type has sides `a` (`from_type`) and `b` (`to_type`) and one of Quarry's three backings:
//!
//! ```json
//! {
//!   "node_types": [
//!     {"name": "Operative", "file": "operatives.parquet", "primary_key": "id",
//!      "properties": {"status": "string", "joined": "date"}}
//!   ],
//!   "link_types": [
//!     {"name": "FOLLOWS", "from_type": "User", "to_type": "User",
//!      "backing": {"dataset": {"file": "follows.parquet", "a": "src", "b": "dst"}}},
//!     {"name": "WORKS_AT", "from_type": "Operative", "to_type": "Station",
//!      "backing": {"foreign_key": {"side": "a", "property": "stationId"}}},
//!     {"name": "LOADOUT", "from_type": "Operative", "to_type": "Equipment",
//!      "backing": {"object": {"type": "Assignment", "a": "operativeId", "b": "equipmentId"}}}
//!   ]
//! }
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use proximadb::core::Kind;
use proximadb::load::{Backing, LinkTypeSpec, NodeTypeSpec, Schema, Side};


#[derive(Deserialize)]
struct SchemaFile {
    node_types: Vec<NodeTypeFile>,
    link_types: Vec<LinkTypeFile>,
}


#[derive(Deserialize)]
struct NodeTypeFile {
    name: String,
    file: String,
    primary_key: String,
    properties: BTreeMap<String, KindFile>,
}


#[derive(Deserialize)]
struct LinkTypeFile {
    name: String,
    from_type: String,
    to_type: String,
    backing: BackingFile,
}


#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum BackingFile {
    /// A join dataset with one key column per side.
    Dataset { file: String, a: String, b: String },

    /// A property on side `a`'s or `b`'s object type holding the other side's key.
    ForeignKey { side: SideFile, property: String },

    /// An intermediary object type with one key property per side.
    Object {
        #[serde(rename = "type")]
        node_type: String,
        a: String,
        b: String,
    },
}


#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum SideFile {
    A,
    B,
}


#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum KindFile {
    String,
    Integer,
    Long,
    Double,
    Boolean,
    Date,
    Timestamp,
}


impl From<KindFile> for Kind {
    fn from(kind: KindFile) -> Self {
        match kind {
            KindFile::String => Kind::String,
            KindFile::Integer => Kind::Integer,
            KindFile::Long => Kind::Long,
            KindFile::Double => Kind::Double,
            KindFile::Boolean => Kind::Boolean,
            KindFile::Date => Kind::Date,
            KindFile::Timestamp => Kind::Timestamp,
        }
    }
}


impl From<BackingFile> for Backing {
    fn from(backing: BackingFile) -> Self {
        match backing {
            BackingFile::Dataset { file, a, b } => Backing::Dataset {
                file,
                from_column: a,
                to_column: b,
            },

            BackingFile::ForeignKey { side, property } => Backing::ForeignKey {
                side: match side {
                    SideFile::A => Side::From,
                    SideFile::B => Side::To,
                },
                property,
            },

            BackingFile::Object { node_type, a, b } => Backing::Object {
                node_type,
                from_property: a,
                to_property: b,
            },
        }
    }
}


pub(crate) fn schema_from(path: &Path) -> Result<Schema, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let file: SchemaFile =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;

    let node_types = file
        .node_types
        .into_iter()
        .map(|node_type| NodeTypeSpec {
            name: node_type.name,
            file: node_type.file,
            primary_key: node_type.primary_key,
            properties: node_type
                .properties
                .into_iter()
                .map(|(name, kind)| (name, kind.into()))
                .collect(),
        })
        .collect();

    let link_types = file
        .link_types
        .into_iter()
        .map(|link_type| LinkTypeSpec {
            name: link_type.name,
            from_type: link_type.from_type,
            to_type: link_type.to_type,
            backing: link_type.backing.into(),
        })
        .collect();

    Ok(Schema {
        node_types,
        link_types,
    })
}
