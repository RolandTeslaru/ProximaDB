//! Walks that return a graph to draw instead of a count: the nodes found, and the links that lead to them.
//!
//! - `neighbourhood`: breadth-first from the starts, recording the link each node was first reached by. That is a
//!   tree: every node shown has exactly one way back to a start, so a drawing shows how each was reached without
//!   every link between them. It stops after `limit` matching nodes, nearest first.
//! - `shortest_path`: the nodes and links of one shortest path between two nodes.
//!
//! Both run on one thread and keep their state in hash maps sized by what they visit, not by the graph: they are
//! for a screen's worth of results. Counting all of them is `walk::reach`.

use std::collections::VecDeque;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::collections::bitset::Bitset;
use crate::core::Direction;
use crate::core::node::GlobalId;
use crate::kinetic::live::{TableRef, View};
use crate::query::walk::{Reach, Through};
use crate::storage::link_type;
use crate::storage::node_type;

/// At most this many nodes are visited per node returned, so a filter that rarely matches can't walk the whole graph.
const VISITS_PER_RESULT: usize = 50;


/// One link, as stored: `from` is a node of the link type's from-type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkRef {
    pub link_type: link_type::Id,
    pub from: GlobalId,
    pub to: GlobalId,
}


/// Why a node is in a subgraph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Start,
    /// A node the query asked for.
    Match,
    /// A node on the way from a start to a match.
    Via,
}


#[derive(Debug, Default)]
pub struct Subgraph {
    pub nodes: Vec<(GlobalId, Role)>,
    pub links: Vec<LinkRef>,
    /// The search stopped at its limit: there are more matches than shown.
    pub truncated: bool,
}


/// Only nodes of `node_type` set in `allowed` (by position within the type) count as matches.
pub struct EndFilter {
    pub node_type: node_type::Id,
    pub allowed: Option<Bitset>,
}


/// Breadth-first from `q.starts`, up to `limit` matches at a distance between `min_steps` and `q.max_steps`.
pub fn neighbourhood(
    view: &View,
    q: &Reach,
    min_steps: u32,
    end: Option<&EndFilter>,
    limit: usize,
) -> Subgraph {
    let tables = view.tables_by_type(q.link_types, q.direction);
    let visit_budget = limit.saturating_mul(VISITS_PER_RESULT).max(limit);

    let mut parent: FxHashMap<GlobalId, (GlobalId, LinkRef)> = FxHashMap::default();
    let mut seen: FxHashSet<GlobalId> = q.starts.iter().copied().collect();
    let mut queue: VecDeque<(GlobalId, u32)> = q.starts.iter().map(|&s| (s, 0)).collect();
    let mut matches = Vec::new();
    let mut truncated = false;

    if min_steps == 0 {
        matches.extend(q.starts.iter().copied().filter(|&s| is_match(view, end, s)));
    }

    'walk: while let Some((u, depth)) = queue.pop_front() {
        if depth == q.max_steps {
            continue;
        }

        for table in &tables[view.type_of(u)] {
            let mut found = Vec::new();

            view.for_each(table, u, |v| {
                if !seen.contains(&v) && allowed_onto(view, q.through, table, v) {
                    found.push(v);
                }
            });

            for v in found {
                if !seen.insert(v) {
                    continue;
                }

                parent.insert(v, (u, link_of(table, u, v)));
                queue.push_back((v, depth + 1));

                if depth + 1 >= min_steps && is_match(view, end, v) {
                    matches.push(v);
                }

                if matches.len() >= limit || seen.len() >= visit_budget {
                    truncated = !queue.is_empty() || matches.len() >= limit;
                    break 'walk;
                }
            }
        }
    }

    matches.truncate(limit);

    tree_of(q.starts, &matches, &parent, truncated)
}


/// One shortest path from `from` to `to`, at most `max_steps` long, stepping only onto nodes `through` allows (the
/// end itself always may be stepped onto). `None` if there is none.
pub fn shortest_path(
    view: &View,
    from: GlobalId,
    to: GlobalId,
    link_types: &[link_type::Id],
    direction: Direction,
    max_steps: u32,
    through: Option<&Through>,
) -> Option<Subgraph> {
    let tables = view.tables_by_type(link_types, direction);

    let mut parent: FxHashMap<GlobalId, (GlobalId, LinkRef)> = FxHashMap::default();
    let mut seen: FxHashSet<GlobalId> = [from].into_iter().collect();
    let mut frontier = vec![from];

    for _ in 0..max_steps {
        if frontier.is_empty() {
            break;
        }

        let mut next = Vec::new();

        for &u in &frontier {
            for table in &tables[view.type_of(u)] {
                view.for_each(table, u, |v| {
                    if seen.contains(&v) {
                        return;
                    }

                    if v != to && !allowed_onto(view, through, table, v) {
                        return;
                    }

                    seen.insert(v);
                    parent.insert(v, (u, link_of(table, u, v)));
                    next.push(v);
                });
            }

            if parent.contains_key(&to) {
                return Some(tree_of(&[from], &[to], &parent, false));
            }
        }

        frontier = next;
    }

    (from == to).then(|| tree_of(&[from], &[], &parent, false))
}


fn allowed_onto(view: &View, through: Option<&Through>, table: &TableRef, v: GlobalId) -> bool {
    if view.is_deleted(v) {
        return false;
    }

    through.is_none_or(|t| t.allows(view, table.other_type, v))
}


fn is_match(view: &View, end: Option<&EndFilter>, node: GlobalId) -> bool {
    let Some(end) = end else {
        return true;
    };

    let (node_type, position) = view.get(node);

    if node_type != end.node_type {
        return false;
    }

    end.allowed
        .as_ref()
        .is_none_or(|allowed| allowed.get(position.index()))
}


/// The link the walk followed from `u` to `v`, the way the link type stores it.
fn link_of(table: &TableRef, u: GlobalId, v: GlobalId) -> LinkRef {
    let (from, to) = if table.forward { (u, v) } else { (v, u) };

    LinkRef {
        link_type: table.link_type,
        from,
        to,
    }
}


/// The starts, the matches, and every node and link on the way from a start to a match.
fn tree_of(
    starts: &[GlobalId],
    matches: &[GlobalId],
    parent: &FxHashMap<GlobalId, (GlobalId, LinkRef)>,
    truncated: bool,
) -> Subgraph {
    let mut roles: FxHashMap<GlobalId, Role> = starts.iter().map(|&s| (s, Role::Start)).collect();
    let mut order: Vec<GlobalId> = starts.to_vec();
    let mut links = Vec::new();

    for &m in matches {
        match roles.get(&m) {
            Some(Role::Start) => continue,
            Some(_) => {
                roles.insert(m, Role::Match);
                continue;
            }
            None => {
                roles.insert(m, Role::Match);
                order.push(m);
            }
        }

        // Walk back towards the start, until a node already in the tree.
        let mut node = m;

        while let Some(&(previous, link)) = parent.get(&node) {
            links.push(link);

            if roles.contains_key(&previous) {
                break;
            }

            roles.insert(previous, Role::Via);
            order.push(previous);
            node = previous;
        }
    }

    Subgraph {
        nodes: order.into_iter().map(|n| (n, roles[&n])).collect(),
        links,
        truncated,
    }
}
