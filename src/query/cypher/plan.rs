//! A parsed query, checked against the graph and run as a walk.
//!
//!   1. Resolve the start node from its label and primary key (`(a:User {nodeId: 42})`).
//!   2. Resolve the link pattern: link types, direction, and how many steps.
//!   3. Turn the conditions into filters: `WHERE` on the end node, and the inline `(r, n | …)` filter on every node
//!      stepped onto. Both need the end node's label, since a condition is evaluated over one node type.
//!   4. Run: a shortest path, a graph of the nearest matches, a count, or both.

use std::collections::HashMap;

use crate::core::node::GlobalId;
use crate::core::{Direction, Value};
use crate::kinetic::live::View;
use crate::query::condition::{self, Condition};
use crate::query::subgraph::{self, EndFilter, Role, Subgraph};
use crate::query::walk::{self, Reach, Through};
use crate::storage::link_type;
use crate::storage::node_type;

use super::parser::{Expr, LinkPattern, Literal, NodePattern, Query, ReturnItem};
use super::{Answer, CypherError, DEFAULT_LIMIT, MAX_DEPTH, MAX_LIMIT};


/// The query, resolved against the graph.
struct Plan {
    start: GlobalId,
    link_types: Vec<link_type::Id>,
    direction: Direction,
    min_steps: u32,
    max_steps: u32,
    end: Option<EndFilter>,
    through: Option<Through>,
}


pub fn run(
    view: &View,
    query: &Query,
    params: &HashMap<String, Value>,
) -> Result<Answer, CypherError> {
    let start = find(view, &query.start, params)?;

    let Some((link, end_node)) = &query.step else {
        return Ok(single_node(start));
    };

    if query.shortest {
        return shortest_path(view, query, start, link, end_node, params);
    }

    if !end_node.properties.is_empty() {
        return Err(CypherError::plain(
            "an end node given by its key needs shortestPath(…); otherwise filter it with WHERE",
        ));
    }

    let plan = resolve(view, query, start, link, end_node, params)?;

    let end_name = end_node.name.as_deref();
    let wants_graph = query
        .returns
        .iter()
        .any(|item| matches!(item, ReturnItem::Name(_)));

    for item in &query.returns {
        check_returned(item, query, end_name)?;
    }

    let count = count_matches(view, &plan);

    let graph = wants_graph.then(|| {
        let limit = query.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize;

        subgraph::neighbourhood(
            view,
            &plan.reach(),
            plan.min_steps,
            plan.end.as_ref(),
            limit,
        )
    });

    Ok(Answer {
        graph,
        count: Some(count),
        path_length: None,
    })
}


impl Plan {
    fn reach(&self) -> Reach<'_> {
        Reach {
            starts: std::slice::from_ref(&self.start),
            link_types: &self.link_types,
            direction: self.direction,
            max_steps: self.max_steps,
            through: self.through.as_ref(),
            cap: None,
        }
    }
}


fn single_node(start: GlobalId) -> Answer {
    Answer {
        graph: Some(Subgraph {
            nodes: vec![(start, Role::Start)],
            links: Vec::new(),
            truncated: false,
        }),
        count: Some(1),
        path_length: None,
    }
}


// ---------------------------------------------------------------- 1. the start node


/// A node given by its label and primary key, e.g. `(a:User {nodeId: 42})`.
fn find(
    view: &View,
    node: &NodePattern,
    params: &HashMap<String, Value>,
) -> Result<GlobalId, CypherError> {
    let graph = view.graph;

    let label = node
        .label
        .as_deref()
        .ok_or_else(|| CypherError::plain("give the node's label, e.g. (a:User {nodeId: 42})"))?;
    let node_type = node_type_named(view, label)?;
    let primary_key = &graph.node_types[node_type].primary_key;

    let [(key_name, key_value)] = node.properties.as_slice() else {
        return Err(CypherError::plain(format!(
            "give the node by its key alone, e.g. ({}:{label} {{{primary_key}: …}})",
            node.name.as_deref().unwrap_or("n")
        )));
    };

    if key_name != primary_key {
        return Err(CypherError::plain(format!(
            "{label} nodes are found by {primary_key}, not {key_name}"
        )));
    }

    let key = key_text(&value_of(key_value, params)?);

    view.lookup(node_type, key.as_bytes())
        .ok_or_else(|| CypherError::plain(format!("no {label} with {primary_key} {key}")))
}


/// A key as stored: text as is, whole numbers without a decimal point.
fn key_text(value: &Value) -> String {
    match value {
        Value::Str(text) => text.clone(),
        Value::Num(n) if n.fract() == 0.0 => format!("{}", *n as i64),
        Value::Num(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
    }
}


fn node_type_named(view: &View, label: &str) -> Result<node_type::Id, CypherError> {
    view.graph
        .node_type(label)
        .ok_or_else(|| CypherError::plain(format!("no node label {label}")))
}


fn value_of(literal: &Literal, params: &HashMap<String, Value>) -> Result<Value, CypherError> {
    match literal {
        Literal::Str(text) => Ok(Value::Str(text.clone())),
        Literal::Num(n) => Ok(Value::Num(*n)),
        Literal::Bool(b) => Ok(Value::Bool(*b)),

        Literal::Param(name) => params
            .get(name)
            .cloned()
            .ok_or_else(|| CypherError::plain(format!("no value for parameter ${name}"))),
    }
}


// ---------------------------------------------------------------- 2. and 3. the link pattern and its filters


fn resolve(
    view: &View,
    query: &Query,
    start: GlobalId,
    link: &LinkPattern,
    end_node: &NodePattern,
    params: &HashMap<String, Value>,
) -> Result<Plan, CypherError> {
    let (min_steps, max_steps) = steps(link)?;
    let link_types = link_types_named(view, &link.types)?;
    let end_type = end_node
        .label
        .as_deref()
        .map(|label| node_type_named(view, label))
        .transpose()?;

    let end_name = end_node.name.as_deref();

    let end_condition = match &query.condition {
        Some(expr) => Some(condition_for(expr, end_name, "WHERE", params)?),
        None => None,
    };

    let through_condition = match &link.filter {
        Some((name, expr)) => Some(condition_for(
            expr,
            Some(name),
            "the link's filter",
            params,
        )?),
        None => None,
    };

    let needs_label = end_condition.is_some() || through_condition.is_some();

    if needs_label && end_type.is_none() {
        return Err(CypherError::plain(
            "label the end node to filter on its properties, e.g. (b:User)",
        ));
    }

    let end = match end_type {
        Some(node_type) => Some(EndFilter {
            node_type,
            allowed: end_condition
                .map(|c| evaluated(view, node_type, c))
                .transpose()?,
        }),

        None => None,
    };

    let through = match (through_condition, end_type) {
        (Some(c), Some(node_type)) => {
            let mut through = Through::new(view.graph, false);
            through.set(node_type, evaluated(view, node_type, c)?);

            Some(through)
        }

        _ => None,
    };

    Ok(Plan {
        start,
        link_types,
        direction: link.direction,
        min_steps,
        max_steps,
        end,
        through,
    })
}


fn steps(link: &LinkPattern) -> Result<(u32, u32), CypherError> {
    let Some((min, max)) = link.length else {
        return Ok((1, 1));
    };

    let max = max.ok_or_else(|| {
        CypherError::plain(format!(
            "give a maximum length, e.g. *1..6 (at most {MAX_DEPTH})"
        ))
    })?;

    if max > MAX_DEPTH {
        return Err(CypherError::plain(format!(
            "a link pattern goes at most {MAX_DEPTH} steps"
        )));
    }

    if min > max {
        return Err(CypherError::plain(format!("*{min}..{max} has no lengths")));
    }

    Ok((min, max))
}


fn link_types_named(view: &View, names: &[String]) -> Result<Vec<link_type::Id>, CypherError> {
    if names.is_empty() {
        return Ok(view.graph.all_link_types());
    }

    names
        .iter()
        .map(|name| {
            view.graph
                .link_type(name)
                .ok_or_else(|| CypherError::plain(format!("no link type {name}")))
        })
        .collect()
}


/// A condition over the node called `name`, which every property in it must refer to.
fn condition_for(
    expr: &Expr,
    name: Option<&str>,
    place: &str,
    params: &HashMap<String, Value>,
) -> Result<Condition, CypherError> {
    let refers_to = |used: &str| -> Result<(), CypherError> {
        if Some(used) == name {
            return Ok(());
        }

        Err(CypherError::plain(format!(
            "{place} can only use properties of {} (found {used})",
            name.unwrap_or("the end node, once it has a name")
        )))
    };

    Ok(match expr {
        Expr::Compare {
            name: used,
            property,
            op,
            value,
        } => {
            refers_to(used)?;

            Condition::compare(property, *op, value_of(value, params)?)
        }

        Expr::IsNull {
            name: used,
            property,
            negated,
        } => {
            refers_to(used)?;

            let has = Condition::HasValue(property.clone());

            if *negated {
                has
            } else {
                Condition::Not(Box::new(has))
            }
        }

        Expr::And(left, right) => Condition::And(vec![
            condition_for(left, name, place, params)?,
            condition_for(right, name, place, params)?,
        ]),

        Expr::Or(left, right) => Condition::Or(vec![
            condition_for(left, name, place, params)?,
            condition_for(right, name, place, params)?,
        ]),

        Expr::Not(inner) => Condition::Not(Box::new(condition_for(inner, name, place, params)?)),
    })
}


/// A condition checked against the node type's properties, then evaluated as of the view.
fn evaluated(
    view: &View,
    node_type: node_type::Id,
    condition: Condition,
) -> Result<crate::collections::Bitset, CypherError> {
    let condition =
        condition::checked(view.graph, node_type, condition).map_err(CypherError::plain)?;

    Ok(condition::evaluate(view, node_type, &condition))
}


fn check_returned(
    item: &ReturnItem,
    query: &Query,
    end_name: Option<&str>,
) -> Result<(), CypherError> {
    let name = match item {
        ReturnItem::Name(name) | ReturnItem::Count { name } => name.as_str(),
    };

    let start_name = query.start.name.as_deref();
    let link_name = query
        .step
        .as_ref()
        .and_then(|(link, _)| link.name.as_deref());
    let path_name = query.path_name.as_deref();

    let known = [start_name, end_name, link_name, path_name].contains(&Some(name));

    if !known {
        return Err(CypherError::plain(format!(
            "{name} isn't named in the MATCH pattern"
        )));
    }

    Ok(())
}


// ---------------------------------------------------------------- 4. running


/// Every match, counted exactly with the parallel walk: nodes at `min_steps` or more that meet the end filter.
fn count_matches(view: &View, plan: &Plan) -> u64 {
    let walked = walk::reach(view, &plan.reach());

    // `reached` is in step order: skip the steps nearer than `min_steps`.
    let too_near: u64 = walked
        .per_step
        .iter()
        .take(plan.min_steps.saturating_sub(1) as usize)
        .sum();

    let start_counts = plan.min_steps == 0 && matches(view, plan.end.as_ref(), plan.start);

    let far_enough = walked.reached[too_near as usize..]
        .iter()
        .filter(|&&node| matches(view, plan.end.as_ref(), node))
        .count() as u64;

    far_enough + start_counts as u64
}


fn matches(view: &View, end: Option<&EndFilter>, node: GlobalId) -> bool {
    let Some(end) = end else {
        return true;
    };

    let (node_type, position) = view.get(node);

    node_type == end.node_type
        && end
            .allowed
            .as_ref()
            .is_none_or(|allowed| allowed.get(position.index()))
}


fn shortest_path(
    view: &View,
    query: &Query,
    start: GlobalId,
    link: &LinkPattern,
    end_node: &NodePattern,
    params: &HashMap<String, Value>,
) -> Result<Answer, CypherError> {
    if end_node.properties.is_empty() {
        return Err(CypherError::plain(
            "shortestPath needs both ends given by their key",
        ));
    }

    if query.condition.is_some() {
        return Err(CypherError::plain(
            "shortestPath takes no WHERE; filter the nodes on the way with (r, n | …)",
        ));
    }

    let end = find(view, end_node, params)?;
    let (_, max_steps) = steps(link)?;
    let link_types = link_types_named(view, &link.types)?;

    let through = match &link.filter {
        Some((name, expr)) => {
            let label = end_node.label.as_deref().expect("find() required a label");
            let node_type = node_type_named(view, label)?;
            let condition = condition_for(expr, Some(name), "the link's filter", params)?;

            let mut through = Through::new(view.graph, false);
            through.set(node_type, evaluated(view, node_type, condition)?);

            Some(through)
        }

        None => None,
    };

    let path = subgraph::shortest_path(
        view,
        start,
        end,
        &link_types,
        link.direction,
        max_steps,
        through.as_ref(),
    );

    let path_length = path.as_ref().map(|p| p.links.len() as u32);

    Ok(Answer {
        count: Some(path.is_some() as u64),
        graph: path,
        path_length,
    })
}
