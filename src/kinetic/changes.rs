//! Property changes since the rebuild: a change log per property; the packed columns stay unchanged.

use std::sync::RwLock;

use crate::collections::blocks::BlockList;
use crate::core::Value;
use crate::storage::graph::Graph;
use crate::storage::node_type;
use crate::storage::property::{self, Column, Property};

#[derive(Clone, Copy)]
pub struct Change {
    pub local: u64,
    pub value: u64,
    pub seq: u64,
}

/// One property's changes since the rebuild, plus string values that the packed column doesn't have yet.
pub struct PropertyLog {
    pub log: BlockList<Change>,
    pub extra_strings: RwLock<Vec<String>>,
}

/// The value types a change reads back as.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Val {
    Num(f64),
    Str(String),
    Bool(bool),
    Missing,
}

pub(crate) fn property_index(graph: &Graph, t: node_type::Id, name: &str) -> usize {
    graph.node_types[t]
        .properties
        .iter()
        .position(|p| p.name == name)
        .unwrap_or_else(|| panic!("no property {name}"))
}

fn coded_values(p: &Property) -> Option<&Vec<String>> {
    match &p.column {
        Column::Coded8 { values, .. } | Column::Coded16 { values, .. } => Some(values),
        _ => None,
    }
}

/// A change's raw value back to a comparable value.
pub(crate) fn decode(p: &Property, log: &PropertyLog, raw: u64) -> Val {
    match &p.column {
        Column::Coded8 { .. } | Column::Coded16 { .. } | Column::Text { .. } => {
            let base = coded_values(p).map_or(0, |v| v.len() as u64);
            if raw < base {
                Val::Str(coded_values(p).unwrap()[raw as usize].clone())
            } else {
                Val::Str(log.extra_strings.read().unwrap()[(raw - base) as usize].clone())
            }
        }
        Column::Double(_) => Val::Num(f64::from_bits(raw)),
        Column::Boolean(_) => Val::Bool(raw != 0),
        _ => Val::Num(raw as i64 as f64),
    }
}

/// A written value to its raw form (writer only).
pub(crate) fn encode(p: &Property, log: &PropertyLog, value: &Value) -> u64 {
    match (&p.column, value) {
        (Column::Coded8 { .. } | Column::Coded16 { .. } | Column::Text { .. }, Value::Str(s)) => {
            let base = coded_values(p).map_or(0, |v| v.len() as u64);
            if let Some(code) = coded_values(p).and_then(|v| v.iter().position(|x| x == s)) {
                return code as u64;
            }
            let mut extra = log.extra_strings.write().unwrap();
            let at = extra.iter().position(|x| x == s).unwrap_or_else(|| {
                extra.push(s.clone());
                extra.len() - 1
            });
            base + at as u64
        }
        (Column::Double(_), Value::Num(x)) => x.to_bits(),
        (Column::Boolean(_), Value::Bool(b)) => *b as u64,
        (Column::Date(_), Value::Str(s)) => property::parse_date(s).expect("date") as i64 as u64,
        (_, Value::Num(x)) => *x as i64 as u64,
        _ => panic!("value does not match the property's type"),
    }
}
