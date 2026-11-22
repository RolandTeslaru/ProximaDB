//! A parsed query, checked against the graph and run as a walk.
//!
//!   1. Split `WHERE` by the node each part is about: the start node or the end node.
//!   2. Resolve the start: one node by its primary key (`(a:User {nodeId: 42})`), or every node of a label that meets
//!      its conditions (`(a:User {status: 'active'})`, `WHERE a.age > 40`).
//!   3. Resolve the link pattern: link types, direction, how many steps, and the filters: on the end node, and the
//!      inline `(r, n | …)` filter on every node stepped onto.
//!   4. Run: a shortest path, a graph of the nearest matches, a count, or both.
//!
//! The walk takes its first step from the starts without marking them seen, so with many starts, a start reached
//! from another start is a match like any other node. A node's distance is its shortest one from any start.

use std::collections::HashMap;

use crate::collections::Bitset;
use crate::core::node::GlobalId;
use crate::core::{Direction, Value};
use crate::kinetic::live::View;
use crate::query::condition::{self, Condition, Op};
use crate::query::subgraph::{self, EndFilter, Role, Subgraph};
use crate::query::walk::{self, Reach, Through};
use crate::storage::link_type;
use crate::storage::node_type;

use super::parser::{Expr, LinkPattern, Literal, NodePattern, Query, ReturnItem};
use super::{Answer, CypherError, DEFAULT_LIMIT, MAX_DEPTH, MAX_LIMIT};


/// The query, resolved against the graph.
struct Plan {
    starts: Vec<GlobalId>,
    link_types: Vec<link_type::Id>,
    direction: Direction,
    min_steps: u32,
    max_steps: u32,
    end: Option<EndFilter>,
    through: Option<Through>,

    /// `WHERE b <> a`: a start is never a match, even when a loop leads back to it.
    skip_starts: bool,
}


/// The parts of `WHERE`, by the node they are about.
#[derive(Default)]
struct Conditions {
    start: Vec<Condition>,
    end: Vec<Condition>,

    /// `b <> a` between the end and the start.
    end_not_start: bool,
}


pub fn run(
    view: &View,
    query: &Query,
    params: &HashMap<String, Value>,
) -> Result<Answer, CypherError> {
    for item in &query.returns {
        check_returned(item, query)?;
    }

    let conditions = split_where(query, params)?;
    let starts = starts_of(view, &query.start, conditions.start, params)?;
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize;

    let Some((link, end_node)) = &query.step else {
        let wants_graph = query
            .returns
            .iter()
            .any(|item| matches!(item, ReturnItem::Name(_)));

        return Ok(nodes_only(&starts, limit, wants_graph));
    };

    if query.shortest {
        return shortest_path(view, query, &starts, link, end_node, params);
    }

    if !end_node.properties.is_empty() {
        return Err(CypherError::plain(
            "an end node given by its key needs shortestPath(…); otherwise filter it with WHERE",
        ));
    }

    counts_only_the_end(query)?;

    let skip_starts = conditions.end_not_start;
    let mut plan = resolve(view, starts, link, end_node, conditions.end, params)?;
    plan.skip_starts = skip_starts;

    let wants_graph = query
        .returns
        .iter()
        .any(|item| matches!(item, ReturnItem::Name(_)));

    let count = count_matches(view, &plan);

    let graph = wants_graph.then(|| {
        subgraph::neighbourhood(
            view,
            &plan.reach(&plan.starts),
            plan.min_steps,
            plan.end.as_ref(),
            plan.skip_starts,
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
    fn reach<'a>(&'a self, starts: &'a [GlobalId]) -> Reach<'a> {
        Reach {
            starts,
            link_types: &self.link_types,
            direction: self.direction,
            max_steps: self.max_steps,
            through: self.through.as_ref(),
            cap: None,
        }
    }
}


/// `MATCH (a…) RETURN a`: the nodes themselves.
fn nodes_only(starts: &[GlobalId], limit: usize, wants_graph: bool) -> Answer {
    let shown = starts
        .iter()
        .take(limit)
        .map(|&node| (node, Role::Match))
        .collect();

    Answer {
        graph: wants_graph.then_some(Subgraph {
            nodes: shown,
            links: Vec::new(),
            truncated: starts.len() > limit,
        }),
        count: Some(starts.len() as u64),
        path_length: None,
    }
}


fn check_returned(item: &ReturnItem, query: &Query) -> Result<(), CypherError> {
    let name = match item {
        ReturnItem::Name(name) | ReturnItem::Count { name } => name.as_str(),
    };

    let start_name = query.start.name.as_deref();
    let end_name = query.step.as_ref().and_then(|(_, end)| end.name.as_deref());
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


/// With a link, `count(…)` counts end nodes; counting the starts that reach one isn't supported yet.
fn counts_only_the_end(query: &Query) -> Result<(), CypherError> {
    let start_name = query.start.name.as_deref();

    for item in &query.returns {
        if let ReturnItem::Count { name } = item
            && Some(name.as_str()) == start_name
        {
            return Err(CypherError::plain(
                "count the end node: counting the starts that reach one isn't supported yet",
            ));
        }
    }

    Ok(())
}


// ---------------------------------------------------------------- 1. WHERE, split by node


/// `WHERE` as a list of parts joined by `AND`, each about the start node or the end node alone.
fn split_where(query: &Query, params: &HashMap<String, Value>) -> Result<Conditions, CypherError> {
    let mut conditions = Conditions::default();

    let Some(expr) = &query.condition else {
        return Ok(conditions);
    };

    let start_name = query.start.name.as_deref();
    let end_name = query.step.as_ref().and_then(|(_, end)| end.name.as_deref());

    for part in conjuncts(expr) {
        if let Expr::Different { left, right } = part {
            let pair = [Some(left.as_str()), Some(right.as_str())];

            if end_name.is_none() || !pair.contains(&start_name) || !pair.contains(&end_name) {
                return Err(CypherError::plain(
                    "<> between two nodes can only compare the start and the end, as in b <> a",
                ));
            }

            conditions.end_not_start = true;
            continue;
        }

        let names = names_in(part);

        let [name] = names.as_slice() else {
            return Err(CypherError::plain(
                "each part of WHERE joined by AND must be about one node; OR and NOT can't mix two nodes",
            ));
        };

        let condition = condition_for(part, Some(name), "WHERE", params)?;

        if Some(*name) == end_name {
            conditions.end.push(condition);
        } else if Some(*name) == start_name {
            conditions.start.push(condition);
        } else {
            return Err(CypherError::plain(format!(
                "WHERE uses {name}, which isn't a node in the pattern"
            )));
        }
    }

    Ok(conditions)
}


fn conjuncts(expr: &Expr) -> Vec<&Expr> {
    match expr {
        Expr::And(left, right) => {
            let mut parts = conjuncts(left);
            parts.extend(conjuncts(right));
            parts
        }

        other => vec![other],
    }
}


/// The node names a condition uses, each once.
fn names_in(expr: &Expr) -> Vec<&str> {
    let mut names = match expr {
        Expr::Compare { name, .. } | Expr::IsNull { name, .. } => vec![name.as_str()],

        Expr::Different { left, right } => vec![left.as_str(), right.as_str()],

        Expr::And(left, right) | Expr::Or(left, right) => {
            let mut both = names_in(left);
            both.extend(names_in(right));
            both
        }

        Expr::Not(inner) => names_in(inner),
    };

    names.sort_unstable();
    names.dedup();

    names
}


// ---------------------------------------------------------------- 2. the starts


/// One node by its primary key, or every node of the label that meets the conditions.
fn starts_of(
    view: &View,
    node: &NodePattern,
    conditions: Vec<Condition>,
    params: &HashMap<String, Value>,
) -> Result<Vec<GlobalId>, CypherError> {
    let label = node.label.as_deref().ok_or_else(|| {
        CypherError::plain("give the start node's label, e.g. (a:User) or (a:User {nodeId: 42})")
    })?;
    let node_type = node_type_named(view, label)?;
    let primary_key = &view.graph.node_types[node_type].primary_key;

    let by_key = matches!(node.properties.as_slice(), [(name, _)] if name == primary_key);

    if by_key {
        let start = find(view, node, params)?;
        let allowed = all_of(view, node_type, conditions)?;
        let position = view.get(start).1.index();
        let passes = allowed.is_none_or(|bits| bits.get(position));

        return Ok(if passes { vec![start] } else { Vec::new() });
    }

    // Inline properties are equalities, like WHERE a.status = 'active'.
    let mut all = conditions;

    for (name, value) in &node.properties {
        if name == primary_key {
            return Err(CypherError::plain(format!(
                "give {label} by its {primary_key} alone, or filter it without the key"
            )));
        }

        all.push(Condition::compare(name, Op::Eq, value_of(value, params)?));
    }

    let allowed = match all.len() {
        0 => None,
        1 => Some(evaluated(
            view,
            node_type,
            all.pop().expect("one condition"),
        )?),
        _ => Some(evaluated(view, node_type, Condition::And(all))?),
    };

    let starts = view
        .nodes_of(node_type)
        .into_iter()
        .enumerate()
        .filter(|&(position, node)| {
            !view.is_deleted(node) && allowed.as_ref().is_none_or(|bits| bits.get(position))
        })
        .map(|(_, node)| node)
        .collect();

    Ok(starts)
}


/// A node given by its label and primary key, e.g. `(a:User {nodeId: 42})`.
fn find(
    view: &View,
    node: &NodePattern,
    params: &HashMap<String, Value>,
) -> Result<GlobalId, CypherError> {
    let label = node
        .label
        .as_deref()
        .ok_or_else(|| CypherError::plain("give the node's label, e.g. (a:User {nodeId: 42})"))?;
    let node_type = node_type_named(view, label)?;
    let primary_key = &view.graph.node_types[node_type].primary_key;

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


// ---------------------------------------------------------------- 3. the link pattern and its filters


fn resolve(
    view: &View,
    starts: Vec<GlobalId>,
    link: &LinkPattern,
    end_node: &NodePattern,
    end_conditions: Vec<Condition>,
    params: &HashMap<String, Value>,
) -> Result<Plan, CypherError> {
    let (min_steps, max_steps) = steps(link)?;
    let link_types = link_types_named(view, &link.types)?;

    let end_type = end_node
        .label
        .as_deref()
        .map(|label| node_type_named(view, label))
        .transpose()?;

    let through_condition = match &link.filter {
        Some((name, expr)) => Some(condition_for(
            expr,
            Some(name),
            "the link's filter",
            params,
        )?),
        None => None,
    };

    if !end_conditions.is_empty() && end_type.is_none() {
        return Err(CypherError::plain(
            "label the end node to filter on its properties, e.g. (b:User)",
        ));
    }

    let end = match end_type {
        Some(node_type) => Some(EndFilter {
            node_type,
            allowed: all_of(view, node_type, end_conditions)?,
        }),

        None => None,
    };

    let through = through_condition
        .map(|condition| path_filter(view, condition))
        .transpose()?;

    Ok(Plan {
        starts,
        link_types,
        direction: link.direction,
        min_steps,
        max_steps,
        end,
        through,
        skip_starts: false,
    })
}


/// The inline `(r, n | …)` filter: evaluated over every node type that has the properties it names. Nodes of other
/// types pass, so a filter on an intermediary type (`n.since >= …` on loans) leaves the people and things around it
/// alone. A filter no node type can evaluate is an error.
fn path_filter(view: &View, condition: Condition) -> Result<Through, CypherError> {
    let graph = view.graph;
    let mut through = Through::new(graph, true);
    let mut applies_to_some = false;

    let mut names = Vec::new();
    condition::properties_in(&condition, &mut names);

    for node_type in graph.node_types.ids() {
        let has_them = names
            .iter()
            .all(|name| graph.property(node_type, name).is_some());

        if !has_them {
            continue;
        }

        through.set(node_type, evaluated(view, node_type, condition.clone())?);
        applies_to_some = true;
    }

    if !applies_to_some {
        return Err(CypherError::plain(format!(
            "no node type has the link filter's properties: {}",
            names.join(", ")
        )));
    }

    Ok(through)
}


/// The conditions joined by AND and evaluated, or `None` when there are none.
fn all_of(
    view: &View,
    node_type: node_type::Id,
    mut conditions: Vec<Condition>,
) -> Result<Option<Bitset>, CypherError> {
    let condition = match conditions.len() {
        0 => return Ok(None),
        1 => conditions.pop().expect("one condition"),
        _ => Condition::And(conditions),
    };

    Ok(Some(evaluated(view, node_type, condition)?))
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

        // Only a part of WHERE joined by AND may compare two nodes; split_where takes those out first.
        Expr::Different { .. } => {
            return Err(CypherError::plain(format!(
                "{place} can't compare two nodes inside OR or NOT; join b <> a with AND"
            )));
        }
    })
}


/// A condition checked against the node type's properties, then evaluated as of the view.
fn evaluated(
    view: &View,
    node_type: node_type::Id,
    condition: Condition,
) -> Result<Bitset, CypherError> {
    let condition =
        condition::checked(view.graph, node_type, condition).map_err(CypherError::plain)?;

    Ok(condition::evaluate(view, node_type, &condition))
}


// ---------------------------------------------------------------- 4. running


/// Every match, counted exactly with the parallel walk: one step from the starts (without marking them), then the
/// rest of the steps from there. Nodes nearer than `min_steps` don't count.
fn count_matches(view: &View, plan: &Plan) -> u64 {
    let end = plan.end.as_ref();
    let starts = starts_to_skip(plan);
    let counts = |node: &&GlobalId| matches(view, end, **node) && !starts.contains(*node);

    let first = if plan.max_steps == 0 {
        Vec::new()
    } else {
        walk::first_step(view, &plan.reach(&plan.starts))
    };

    if plan.min_steps == 0 {
        return count_including_starts(view, plan, &first);
    }

    let mut count = 0;

    if plan.min_steps <= 1 {
        count += first.iter().filter(counts).count() as u64;
    }

    if plan.max_steps <= 1 {
        return count;
    }

    let rest = Reach {
        max_steps: plan.max_steps - 1,
        ..plan.reach(&first)
    };

    let walked = walk::reach(view, &rest);

    // `reached` is in step order, from step 2: skip the steps nearer than `min_steps`.
    let too_near: u64 = walked
        .per_step
        .iter()
        .take(plan.min_steps.saturating_sub(2) as usize)
        .sum();

    count += walked.reached[too_near as usize..]
        .iter()
        .filter(counts)
        .count() as u64;

    count
}


/// With `*0..`, the starts are matches too, and a start may also be a step from another: count each node once.
fn count_including_starts(view: &View, plan: &Plan, first: &[GlobalId]) -> u64 {
    let mut found: rustc_hash::FxHashSet<GlobalId> =
        plan.starts.iter().chain(first).copied().collect();

    if plan.max_steps > 1 {
        let rest = Reach {
            max_steps: plan.max_steps - 1,
            ..plan.reach(first)
        };

        found.extend(walk::reach(view, &rest).reached);
    }

    let starts = starts_to_skip(plan);

    found
        .into_iter()
        .filter(|&node| matches(view, plan.end.as_ref(), node) && !starts.contains(&node))
        .count() as u64
}


/// The starts, when `b <> a` says they never count; otherwise nothing.
fn starts_to_skip(plan: &Plan) -> rustc_hash::FxHashSet<GlobalId> {
    if plan.skip_starts {
        plan.starts.iter().copied().collect()
    } else {
        rustc_hash::FxHashSet::default()
    }
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
    starts: &[GlobalId],
    link: &LinkPattern,
    end_node: &NodePattern,
    params: &HashMap<String, Value>,
) -> Result<Answer, CypherError> {
    let [start] = starts else {
        return Err(CypherError::plain(
            "shortestPath needs both ends given by their key",
        ));
    };

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
            let condition = condition_for(expr, Some(name), "the link's filter", params)?;

            Some(path_filter(view, condition)?)
        }

        None => None,
    };

    let path = subgraph::shortest_path(
        view,
        *start,
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
