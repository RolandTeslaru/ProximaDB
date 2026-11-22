//! Everything a request names or sends, checked before it reaches the engine: a release build aborts on a panic, so
//! a wrong name or value must be a 400, never a panic.

use std::collections::HashSet;

use axum::http::StatusCode;

use proximadb::View;
use proximadb::core::node::GlobalId;
use proximadb::core::{Kind, Value};
use proximadb::query::condition::{self, Condition, Op};
use proximadb::query::walk::Through;
use proximadb::storage::property::{self, Column, Property};
use proximadb::storage::{Graph, link_type, node_type};

use crate::api::{ApiError, Edit, Filter, JsonCondition, JsonValue, NodeRef, bad};


pub(crate) fn node_type_of(graph: &Graph, name: &str) -> Result<node_type::Id, ApiError> {
    graph
        .node_type(name)
        .ok_or_else(|| bad(format!("no node type {name}")))
}


/// The named link types, or all of them when none are named.
pub(crate) fn link_types_of(
    graph: &Graph,
    names: &[String],
) -> Result<Vec<link_type::Id>, ApiError> {
    if names.is_empty() {
        return Ok(graph.all_link_types());
    }

    names
        .iter()
        .map(|name| {
            graph
                .link_type(name)
                .ok_or_else(|| bad(format!("no link type {name}")))
        })
        .collect()
}


pub(crate) fn find(view: &View, node: &NodeRef) -> Result<GlobalId, ApiError> {
    let node_type = node_type_of(view.graph, &node.node_type)?;

    view.lookup(node_type, node.key.as_bytes()).ok_or_else(|| {
        ApiError(
            StatusCode::NOT_FOUND,
            format!("no {} with key {}", node.node_type, node.key),
        )
    })
}


fn property_of<'g>(
    graph: &'g Graph,
    node_type: node_type::Id,
    name: &str,
) -> Result<&'g Property, ApiError> {
    graph
        .property(node_type, name)
        .ok_or_else(|| bad(format!("no property {name}")))
}


/// A value checked against a property's kind, ready to write or compare. Dates may be given as `YYYY-MM-DD`.
pub(crate) fn value_for(property: &Property, value: &JsonValue) -> Result<Value, ApiError> {
    let name = &property.name;

    match (property.kind, value) {
        (Kind::String, JsonValue::Str(_)) | (Kind::Boolean, JsonValue::Bool(_)) => Ok(value.into()),

        (Kind::Date, JsonValue::Str(text)) => property::parse_date(text)
            .map(|days| Value::Num(days as f64))
            .ok_or_else(|| bad(format!("{name}: not a date: {text}"))),

        (
            Kind::Integer | Kind::Long | Kind::Double | Kind::Date | Kind::Timestamp,
            JsonValue::Num(_),
        ) => Ok(value.into()),

        (kind, _) => Err(bad(format!(
            "{name}: wrong type of value for a {kind:?} property"
        ))),
    }
}


fn op_of(name: &str) -> Result<Op, ApiError> {
    match name {
        "eq" => Ok(Op::Eq),
        "ne" => Ok(Op::Ne),
        "lt" => Ok(Op::Lt),
        "le" => Ok(Op::Le),
        "gt" => Ok(Op::Gt),
        "ge" => Ok(Op::Ge),
        other => Err(bad(format!("unknown op {other}"))),
    }
}


pub(crate) fn condition_for(
    graph: &Graph,
    node_type: node_type::Id,
    condition: &JsonCondition,
) -> Result<Condition, ApiError> {
    let all = |parts: &[JsonCondition]| -> Result<Vec<Condition>, ApiError> {
        parts
            .iter()
            .map(|part| condition_for(graph, node_type, part))
            .collect()
    };

    match condition {
        JsonCondition::Compare {
            property: name,
            op,
            value,
        } => {
            let property = property_of(graph, node_type, name)?;
            let op = op_of(op)?;

            // Coded strings and booleans are stored without an order.
            let equality_only = matches!(
                property.column,
                Column::Coded8 { .. } | Column::Coded16 { .. } | Column::Boolean(_)
            );

            if equality_only && !matches!(op, Op::Eq | Op::Ne) {
                return Err(bad(format!("{name}: only eq and ne")));
            }

            Ok(Condition::compare(name, op, value_for(property, value)?))
        }

        JsonCondition::Has { has } => {
            property_of(graph, node_type, has)?;

            Ok(Condition::HasValue(has.clone()))
        }

        JsonCondition::And { and } => Ok(Condition::And(all(and)?)),

        JsonCondition::Or { or } => Ok(Condition::Or(all(or)?)),

        JsonCondition::Not { not } => {
            let inner = condition_for(graph, node_type, not)?;

            Ok(Condition::Not(Box::new(inner)))
        }
    }
}


/// Checked filters, as (node type, condition).
pub(crate) fn filters_for(
    graph: &Graph,
    filters: &[Filter],
) -> Result<Vec<(node_type::Id, Condition)>, ApiError> {
    filters
        .iter()
        .map(|filter| {
            let node_type = node_type_of(graph, &filter.node_type)?;
            let condition = condition_for(graph, node_type, &filter.condition)?;

            Ok((node_type, condition))
        })
        .collect()
}


/// The path filter of a walk: each condition evaluated over its node type, as of `view`.
pub(crate) fn through_for(
    view: &View,
    filters: Vec<(node_type::Id, Condition)>,
    others_allowed: bool,
) -> Option<Through> {
    if filters.is_empty() {
        return None;
    }

    let mut through = Through::new(view.graph, others_allowed);

    for (node_type, condition) in filters {
        through.set(node_type, condition::evaluate(view, node_type, &condition));
    }

    Some(through)
}


/// One edit of an action, checked: every name resolved and every value matched to its property.
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


/// Checks a whole action against `view` before any of it is applied, so a bad edit leaves nothing half done.
pub(crate) fn check_action(view: &View, edits: &[Edit]) -> Result<Vec<Checked>, String> {
    let mut checker = ActionChecker {
        view,
        added: HashSet::new(),
    };

    edits
        .iter()
        .map(|edit| checker.check(edit).map_err(|error| error.1))
        .collect()
}


struct ActionChecker<'v, 'g> {
    view: &'v View<'g>,
    /// Nodes added by earlier edits of the same action, which later edits may name.
    added: HashSet<(node_type::Id, String)>,
}


impl ActionChecker<'_, '_> {
    fn check(&mut self, edit: &Edit) -> Result<Checked, ApiError> {
        match edit {
            Edit::AddNode {
                node_type,
                key,
                properties,
            } => self.add_node(node_type, key, properties),

            Edit::SetProperty {
                node,
                property,
                value,
            } => self.set_property(node, property, value),

            Edit::DeleteNode { node } => {
                let (_, target) = self.target(node)?;

                Ok(Checked::DeleteNode(target))
            }

            Edit::AddLink {
                link_type,
                from,
                to,
            } => {
                let (link_type, from, to) = self.link(link_type, from, to)?;

                Ok(Checked::AddLink(link_type, from, to))
            }

            Edit::RemoveLink {
                link_type,
                from,
                to,
            } => {
                let (link_type, from, to) = self.link(link_type, from, to)?;

                Ok(Checked::RemoveLink(link_type, from, to))
            }
        }
    }


    fn add_node(
        &mut self,
        type_name: &str,
        key: &str,
        properties: &std::collections::HashMap<String, JsonValue>,
    ) -> Result<Checked, ApiError> {
        let graph = self.view.graph;
        let node_type = node_type_of(graph, type_name)?;
        let new = (node_type, key.to_string());

        if self.view.lookup(node_type, key.as_bytes()).is_some() || self.added.contains(&new) {
            return Err(bad(format!("{type_name} {key} already exists")));
        }

        let mut values = Vec::with_capacity(properties.len());

        for (name, value) in properties {
            let property = property_of(graph, node_type, name)?;

            values.push((name.clone(), value_for(property, value)?));
        }

        self.added.insert(new);

        Ok(Checked::AddNode(node_type, key.to_string(), values))
    }


    fn set_property(
        &self,
        node: &NodeRef,
        name: &str,
        value: &JsonValue,
    ) -> Result<Checked, ApiError> {
        let (node_type, target) = self.target(node)?;
        let property = property_of(self.view.graph, node_type, name)?;

        Ok(Checked::SetProperty(
            target,
            name.to_string(),
            value_for(property, value)?,
        ))
    }


    /// A link's type and both ends, checked against the node types the link type joins.
    fn link(
        &self,
        name: &str,
        from: &NodeRef,
        to: &NodeRef,
    ) -> Result<(link_type::Id, Target, Target), ApiError> {
        let graph = self.view.graph;

        let link_type = graph
            .link_type(name)
            .ok_or_else(|| bad(format!("no link type {name}")))?;
        let storage = &graph.link_types[link_type];

        let (from_type, from) = self.target(from)?;
        let (to_type, to) = self.target(to)?;

        if from_type != storage.from_type || to_type != storage.to_type {
            let from_name = &graph.node_types[storage.from_type].name;
            let to_name = &graph.node_types[storage.to_type].name;

            return Err(bad(format!("{name} joins {from_name} to {to_name}")));
        }

        Ok((link_type, from, to))
    }


    fn target(&self, node: &NodeRef) -> Result<(node_type::Id, Target), ApiError> {
        let node_type = node_type_of(self.view.graph, &node.node_type)?;

        if self.added.contains(&(node_type, node.key.clone())) {
            return Ok((node_type, Target::Added(node_type, node.key.clone())));
        }

        let existing = find(self.view, node)?;

        Ok((node_type, Target::Existing(existing)))
    }
}
