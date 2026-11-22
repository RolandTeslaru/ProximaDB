//! The schema file: node types with their properties and source files, link types with their join files.
//!
//! ```json
//! {
//!   "node_types": [{"name": "User", "file": "users.csv", "primary_key": "id", "properties": {"status": "string"}}],
//!   "link_types": [{"name": "FOLLOWS", "from_type": "User", "to_type": "User", "file": "follows.csv",
//!                   "from_column": "src", "to_column": "dst"}]
//! }
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use proximadb::core::Kind;
use proximadb::load::{LinkTypeSpec, NodeTypeSpec, Schema};


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
    file: String,
    from_column: String,
    to_column: String,
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
            file: link_type.file,
            from_column: link_type.from_column,
            to_column: link_type.to_column,
        })
        .collect();

    Ok(Schema {
        node_types,
        link_types,
    })
}
