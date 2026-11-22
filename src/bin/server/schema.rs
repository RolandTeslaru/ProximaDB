//! The schema file: node types with their properties and source files, link types with their join files.

use std::path::PathBuf;

use proximadb::core::Kind;
use proximadb::load::{LinkTypeSpec, NodeTypeSpec, Schema};

pub(crate) fn schema_from(path: &PathBuf) -> Result<Schema, String> {
    let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let s = |v: &serde_json::Value, k: &str| v[k].as_str().map(str::to_string).ok_or(format!("missing {k}"));
    let kind = |k: &str| match k {
        "string" => Ok(Kind::String),
        "integer" => Ok(Kind::Integer),
        "long" => Ok(Kind::Long),
        "double" => Ok(Kind::Double),
        "boolean" => Ok(Kind::Boolean),
        "date" => Ok(Kind::Date),
        "timestamp" => Ok(Kind::Timestamp),
        other => Err(format!("unknown property kind {other}")),
    };
    let node_types = doc["node_types"]
        .as_array()
        .ok_or("missing node_types")?
        .iter()
        .map(|n| {
            Ok(NodeTypeSpec {
                name: s(n, "name")?,
                file: s(n, "file")?,
                primary_key: s(n, "primary_key")?,
                properties: n["properties"]
                    .as_object()
                    .ok_or("missing properties")?
                    .iter()
                    .map(|(name, k)| Ok((name.clone(), kind(k.as_str().ok_or("kind must be text")?)?)))
                    .collect::<Result<_, String>>()?,
            })
        })
        .collect::<Result<_, String>>()?;
    let link_types = doc["link_types"]
        .as_array()
        .ok_or("missing link_types")?
        .iter()
        .map(|l| {
            Ok(LinkTypeSpec {
                name: s(l, "name")?,
                from_type: s(l, "from_type")?,
                to_type: s(l, "to_type")?,
                file: s(l, "file")?,
                from_column: s(l, "from_column")?,
                to_column: s(l, "to_column")?,
            })
        })
        .collect::<Result<_, String>>()?;
    Ok(Schema { node_types, link_types })
}
