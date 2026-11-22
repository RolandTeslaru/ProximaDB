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
use crate::collections::index_vec::IndexVec;
use crate::core::Direction;
use crate::core::node::GlobalId;
use crate::kinetic::live::{TableRef, View};
use crate::query::walk::{Reach, Through, reverse};
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
///
/// The starts aren't marked as seen before the first step, so a start reached from another start (or from itself,
/// around a loop) counts like any other node. In the drawing, a start appears only if a match is reached from it,
/// unless there is a single start, which always appears. With `skip_starts`, a start is never a match.
pub fn neighbourhood(
    view: &View,
    q: &Reach,
    min_steps: u32,
    end: Option<&EndFilter>,
    skip_starts: bool,
    limit: usize,
) -> Subgraph {
    let tables = view.tables_by_type(q.link_types, q.direction);
    let visit_budget = limit.saturating_mul(VISITS_PER_RESULT).max(limit);

    let mut parent: FxHashMap<GlobalId, (GlobalId, LinkRef)> = FxHashMap::default();
    let mut seen: FxHashSet<GlobalId> = FxHashSet::default();
    let mut queue: VecDeque<(GlobalId, u32)> = q.starts.iter().map(|&s| (s, 0)).collect();
    let mut matches = Vec::new();
    let mut truncated = false;

    let is_start: FxHashSet<GlobalId> = if skip_starts {
        q.starts.iter().copied().collect()
    } else {
        FxHashSet::default()
    };

    if min_steps == 0 && !skip_starts {
        matches.extend(
            q.starts
                .iter()
                .copied()
                .filter(|&s| is_match(view, end, s))
                .take(limit),
        );
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

                if depth + 1 >= min_steps && is_match(view, end, v) && !is_start.contains(&v) {
                    matches.push(v);
                }

                if matches.len() >= limit || seen.len() >= visit_budget {
                    truncated = true;
                    break 'walk;
                }
            }
        }
    }

    matches.truncate(limit);

    let always_shown = if q.starts.len() == 1 { q.starts } else { &[] };

    tree_of(always_shown, &matches, &parent, truncated)
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
    if from == to {
        return Some(tree_of(&[from], &[], &FxHashMap::default(), false));
    }

    // Two searches, as in `walk::path_length`: one from each end, until they meet.
    let forward_tables = view.tables_by_type(link_types, direction);
    let backward_tables = view.tables_by_type(link_types, reverse(direction));

    let mut start_side = Side::new(from);
    let mut end_side = Side::new(to);
    let mut meeting: Meeting = None;

    loop {
        let steps = start_side.level + end_side.level;

        // No meeting found later can be shorter than one step more than both searches have gone.
        let found_shortest = meeting.is_some_and(|(_, length)| length <= steps + 1);

        if found_shortest || steps >= max_steps {
            break;
        }

        if start_side.front.is_empty() || end_side.front.is_empty() {
            break;
        }

        // Grow the side whose current step is smaller.
        if start_side.front.len() <= end_side.front.len() {
            grow(
                view,
                &forward_tables,
                through,
                &mut start_side,
                &end_side,
                &mut meeting,
            );
        } else {
            grow(
                view,
                &backward_tables,
                through,
                &mut end_side,
                &start_side,
                &mut meeting,
            );
        }
    }

    let (meet, length) = meeting?;

    if length > max_steps {
        return None;
    }

    let path = path_through(meet, &start_side, &end_side);

    Some(tree_of(&[from], &[to], &path, false))
}


/// One end's search in `shortest_path`.
struct Side {
    /// Steps from this side's end to each node it reached.
    distance: FxHashMap<GlobalId, u32>,

    /// The node each reached node was reached from, and the link between them.
    reached_from: FxHashMap<GlobalId, (GlobalId, LinkRef)>,

    /// The nodes reached in the last step.
    front: Vec<GlobalId>,

    /// How many steps this side has gone.
    level: u32,
}


impl Side {
    fn new(end: GlobalId) -> Self {
        Side {
            distance: [(end, 0)].into_iter().collect(),
            reached_from: FxHashMap::default(),
            front: vec![end],
            level: 0,
        }
    }
}


/// The shortest meeting of the two searches so far: the node they met at, and the path's length through it.
type Meeting = Option<(GlobalId, u32)>;


/// Takes one step from every node in `side`'s front, noting where it meets `other`.
fn grow(
    view: &View,
    tables: &IndexVec<node_type::Id, Vec<TableRef>>,
    through: Option<&Through>,
    side: &mut Side,
    other: &Side,
    meeting: &mut Meeting,
) {
    let Side {
        distance,
        reached_from,
        front,
        level,
    } = side;

    let mut next = Vec::new();

    for &u in front.iter() {
        for table in &tables[view.type_of(u)] {
            view.for_each(table, u, |v| {
                if distance.contains_key(&v) {
                    return;
                }

                let theirs = other.distance.get(&v).copied();

                // The other search's own end is always allowed onto; every other node must be.
                let is_other_end = theirs == Some(0);

                if !is_other_end && !allowed_onto(view, through, table, v) {
                    return;
                }

                distance.insert(v, *level + 1);
                reached_from.insert(v, (u, link_of(table, u, v)));
                next.push(v);

                if let Some(theirs) = theirs {
                    let length = *level + 1 + theirs;

                    if meeting.is_none_or(|(_, best)| length < best) {
                        *meeting = Some((v, length));
                    }
                }
            });
        }
    }

    *front = next;
    *level += 1;
}


/// The path through `meet`, as `tree_of` takes it: each node on it with the node before it and the link between.
fn path_through(
    meet: GlobalId,
    start_side: &Side,
    end_side: &Side,
) -> FxHashMap<GlobalId, (GlobalId, LinkRef)> {
    let mut before = FxHashMap::default();

    // From the meeting back to the start, as the start's search reached each node.
    let mut node = meet;

    while let Some(&step) = start_side.reached_from.get(&node) {
        before.insert(node, step);
        node = step.0;
    }

    // From the meeting on to the end: the end's search reached `node` from `next`, so `node` comes before `next`.
    let mut node = meet;

    while let Some(&(next, link)) = end_side.reached_from.get(&node) {
        before.insert(next, (node, link));
        node = next;
    }

    before
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


/// The matches, every node and link on the way back to the start each was reached from, and those starts. A node
/// with no parent is a start; `always_shown` starts appear even with no match.
fn tree_of(
    always_shown: &[GlobalId],
    matches: &[GlobalId],
    parent: &FxHashMap<GlobalId, (GlobalId, LinkRef)>,
    truncated: bool,
) -> Subgraph {
    let mut roles: FxHashMap<GlobalId, Role> =
        always_shown.iter().map(|&s| (s, Role::Start)).collect();
    let mut order: Vec<GlobalId> = always_shown.to_vec();
    let mut links = Vec::new();

    for &m in matches {
        let already_shown = roles.insert(m, Role::Match).is_some();

        if already_shown {
            continue;
        }

        order.push(m);

        // Walk back towards the start, until a node already in the drawing.
        let mut node = m;

        while let Some(&(previous, link)) = parent.get(&node) {
            links.push(link);

            if roles.contains_key(&previous) {
                break;
            }

            let role = if parent.contains_key(&previous) {
                Role::Via
            } else {
                Role::Start
            };

            roles.insert(previous, role);
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
