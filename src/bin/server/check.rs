//! Everything a request names or sends, checked before it reaches the engine: a release build aborts on a panic, so
//! a wrong name or value must be a 400, never a panic.

use std::collections::HashMap;

use axum::http::StatusCode;

use proximadb::core::Kind;
use proximadb::core::Value;
use proximadb::core::node::GlobalId;
use proximadb::query::condition::{self, Condition, Op};
use proximadb::query::walk::Through;
use proximadb::storage::property::{self, Column};
use proximadb::storage::{Graph, link_type, node_type};
use proximadb::View;

use crate::api::{ApiError, Edit, Filter, JsonCondition, JsonValue, NodeRef, bad};

pub(crate) fn node_type_of(graph: &Graph, name: &str) -> Result<node_type::Id, ApiError> {
    graph.node_type(name).ok_or_else(|| bad(format!("no node type {name}")))
}

pub(crate) fn link_types_of(graph: &Graph, names: &[String]) -> Result<Vec<link_type::Id>, ApiError> {
    if names.is_empty() {
        return Ok(graph.all_link_types());
    }
    names
        .iter()
        .map(|n| graph.link_type(n).ok_or_else(|| bad(format!("no link type {n}"))))
        .collect()
}

pub(crate) fn find(view: &View, r: &NodeRef) -> Result<GlobalId, ApiError> {
    let t = node_type_of(view.graph, &r.node_type)?;
    view.lookup(t, r.key.as_bytes())
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no {} with key {}", r.node_type, r.key)))
}

/// A value checked against a property's kind, ready to write or compare.
pub(crate) fn value_for(kind: Kind, name: &str, v: &JsonValue) -> Result<Value, ApiError> {
    match (kind, v) {
        (Kind::String, JsonValue::Str(_)) | (Kind::Boolean, JsonValue::Bool(_)) => Ok(v.into()),
        (Kind::Date, JsonValue::Str(s)) => property::parse_date(s)
            .map(|d| Value::Num(d as f64))
            .ok_or_else(|| bad(format!("{name}: not a date: {s}"))),
        (Kind::Integer | Kind::Long | Kind::Double | Kind::Date | Kind::Timestamp, JsonValue::Num(_)) => Ok(v.into()),
        _ => Err(bad(format!("{name}: wrong type of value for a {kind:?} property"))),
    }
}

pub(crate) fn condition_for(graph: &Graph, t: node_type::Id, c: &JsonCondition) -> Result<Condition, ApiError> {
    Ok(match c {
        JsonCondition::Compare { property: name, op, value } => {
            let p = graph
                .property(t, name)
                .ok_or_else(|| bad(format!("no property {name}")))?;
            let op = match op.as_str() {
                "eq" => Op::Eq,
                "ne" => Op::Ne,
                "lt" => Op::Lt,
                "le" => Op::Le,
                "gt" => Op::Gt,
                "ge" => Op::Ge,
                other => return Err(bad(format!("unknown op {other}"))),
            };
            let equality_only = matches!(p.column, Column::Coded8 { .. } | Column::Coded16 { .. } | Column::Boolean(_));
            if equality_only && !matches!(op, Op::Eq | Op::Ne) {
                return Err(bad(format!("{name}: only eq and ne")));
            }
            Condition::compare(name, op, value_for(p.kind, name, value)?)
        }
        JsonCondition::Has { has } => {
            graph.property(t, has).ok_or_else(|| bad(format!("no property {has}")))?;
            Condition::HasValue(has.clone())
        }
        JsonCondition::And { and } => Condition::And(and.iter().map(|c| condition_for(graph, t, c)).collect::<Result<_, _>>()?),
        JsonCondition::Or { or } => Condition::Or(or.iter().map(|c| condition_for(graph, t, c)).collect::<Result<_, _>>()?),
        JsonCondition::Not { not } => Condition::Not(Box::new(condition_for(graph, t, not)?)),
    })
}

/// Checked filters, as (type, condition).
pub(crate) fn filters_for(graph: &Graph, filters: &[Filter]) -> Result<Vec<(node_type::Id, Condition)>, ApiError> {
    filters
        .iter()
        .map(|f| {
            let t = node_type_of(graph, &f.node_type)?;
            Ok((t, condition_for(graph, t, &f.condition)?))
        })
        .collect()
}

pub(crate) fn through_for(view: &View, filters: Vec<(node_type::Id, Condition)>, others: bool) -> Option<Through> {
    if filters.is_empty() {
        return None;
    }
    let mut through = Through::new(view.graph, others);
    for (t, c) in filters {
        through.set(t, condition::evaluate(view, t, &c));
    }
    Some(through)
}

/// Checks a whole action against the current generation before any of it is applied, so a bad edit leaves nothing
/// half done. Returns the checked edits.
pub(crate) enum Checked {
    AddNode(node_type::Id, String, Vec<(String, Value)>),
    SetProperty(Target, String, Value),
    DeleteNode(Target),
    AddLink(link_type::Id, Target, Target),
    RemoveLink(link_type::Id, Target, Target),
}

/// A node an edit names: one that exists, or one added earlier in the same action.
#[derive(Clone)]
pub(crate) enum Target {
    Existing(GlobalId),
    Added(node_type::Id, String),
}

pub(crate) fn check_action(view: &View, edits: &[Edit]) -> Result<Vec<Checked>, String> {
    let graph = view.graph;
    let msg = |e: ApiError| e.1;
    let mut added: HashMap<(node_type::Id, String), ()> = HashMap::new();
    let target = |r: &NodeRef, added: &HashMap<(node_type::Id, String), ()>| -> Result<(node_type::Id, Target), String> {
        let t = node_type_of(graph, &r.node_type).map_err(msg)?;
        if added.contains_key(&(t, r.key.clone())) {
            return Ok((t, Target::Added(t, r.key.clone())));
        }
        match view.lookup(t, r.key.as_bytes()) {
            Some(g) => Ok((t, Target::Existing(g))),
            None => Err(format!("no {} with key {}", r.node_type, r.key)),
        }
    };
    let mut out = Vec::with_capacity(edits.len());
    for e in edits {
        out.push(match e {
            Edit::AddNode { node_type, key, properties } => {
                let t = node_type_of(graph, node_type).map_err(msg)?;
                if view.lookup(t, key.as_bytes()).is_some() || added.contains_key(&(t, key.clone())) {
                    return Err(format!("{node_type} {key} already exists"));
                }
                let mut props = Vec::new();
                for (name, v) in properties {
                    let p = graph.property(t, name).ok_or(format!("no property {name}"))?;
                    props.push((name.clone(), value_for(p.kind, name, v).map_err(msg)?));
                }
                added.insert((t, key.clone()), ());
                Checked::AddNode(t, key.clone(), props)
            }
            Edit::SetProperty { node, property: name, value } => {
                let (t, n) = target(node, &added)?;
                let p = graph.property(t, name).ok_or(format!("no property {name}"))?;
                Checked::SetProperty(n, name.clone(), value_for(p.kind, name, value).map_err(msg)?)
            }
            Edit::DeleteNode { node } => Checked::DeleteNode(target(node, &added)?.1),
            Edit::AddLink { link_type: name, from, to } | Edit::RemoveLink { link_type: name, from, to } => {
                let r = graph.link_type(name).ok_or(format!("no link type {name}"))?;
                let rel = &graph.link_types[r];
                let (ft, f) = target(from, &added)?;
                let (tt, to) = target(to, &added)?;
                if ft != rel.from_type || tt != rel.to_type {
                    return Err(format!("{name} joins {} to {}", graph.node_types[rel.from_type].name, graph.node_types[rel.to_type].name));
                }
                if matches!(e, Edit::AddLink { .. }) { Checked::AddLink(r, f, to) } else { Checked::RemoveLink(r, f, to) }
            }
        });
    }
    Ok(out)
}
