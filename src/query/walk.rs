//! Walks: `reach` (step by step, push or pull, multithreaded) and `path_length` (two-sided).

use std::cell::RefCell;

use rayon::prelude::*;

use crate::collections::IndexVec;
use crate::collections::bitset::{AtomicBitset, Bitset};
use crate::core::Direction;
use crate::core::node::GlobalId;
use crate::kinetic::draft::{TableRef, View};
use crate::storage::graph::Graph;
use crate::storage::link_type;
use crate::storage::node_type;

/// A step of at least this many nodes is split across threads.
pub const PARALLEL_MIN: usize = 1024;
const CHUNK: usize = 256;
/// Direction switching (Beamer et al., 2012): pull when the step's connections exceed 1/ALPHA of the unvisited
/// nodes' connections; back to push when the step drops below 1/BETA of the nodes.
const ALPHA: u64 = 14;
const BETA: u64 = 24;

/// Conditions checked on every node a walk steps onto: one yes/no list per node type, in that type's local ids.
pub struct Through {
    per_type: IndexVec<node_type::Id, Option<Bitset>>,
    others_allowed: bool,
}

impl Through {
    pub fn new(graph: &Graph, others_allowed: bool) -> Self {
        Self {
            per_type: IndexVec::from_fn(graph.node_types.len(), |_| None),
            others_allowed,
        }
    }

    pub fn set(&mut self, t: node_type::Id, allowed: Bitset) {
        self.per_type[t] = Some(allowed);
    }

    /// Whether node `g` of type `t` meets the conditions.
    #[inline]
    pub fn allows(&self, view: &View, t: node_type::Id, g: GlobalId) -> bool {
        match &self.per_type[t] {
            Some(b) => b.get(view.position(t, g).index()),
            None => self.others_allowed,
        }
    }
}

pub struct Reach<'a> {
    pub starts: &'a [GlobalId],
    pub link_types: &'a [link_type::Id],
    pub direction: Direction,
    pub max_steps: u32,
    pub through: Option<&'a Through>,
    /// Stop after the step in which this many nodes have been reached.
    pub cap: Option<u64>,
}

pub struct ReachResult {
    /// Every node reached, without the starts, as global ids.
    pub reached: Vec<GlobalId>,
    pub per_step: Vec<u64>,
    pub capped: bool,
    pub pull_steps: u32,
    /// Connections read (push: every connection of the step's nodes; pull: every one checked before a match).
    pub scanned: u64,
}

pub(crate) fn reverse(d: Direction) -> Direction {
    match d {
        Direction::Forward => Direction::Backward,
        Direction::Backward => Direction::Forward,
        Direction::Both => Direction::Both,
    }
}

#[inline]
fn degree(view: &View, tables: &IndexVec<node_type::Id, Vec<TableRef>>, g: GlobalId) -> u64 {
    tables[view.type_of(g)]
        .iter()
        .map(|t| view.degree(t, g))
        .sum()
}

fn degree_sum(
    view: &View,
    tables: &IndexVec<node_type::Id, Vec<TableRef>>,
    nodes: &[GlobalId],
) -> u64 {
    if nodes.len() >= PARALLEL_MIN && rayon::current_num_threads() > 1 {
        nodes
            .par_iter()
            .with_min_len(CHUNK)
            .map(|&g| degree(view, tables, g))
            .sum()
    } else {
        nodes.iter().map(|&g| degree(view, tables, g)).sum()
    }
}

/// Everyone reachable from the starts within `max_steps`, step by step. Each node is handled once.
pub fn reach(view: &View, q: &Reach) -> ReachResult {
    let n = view.node_count();
    let out_tables = view.tables_by_type(q.link_types, q.direction);
    let in_tables = view.tables_by_type(q.link_types, reverse(q.direction));
    let total_links: u64 = out_tables
        .iter()
        .flatten()
        .map(|t| t.base.len() as u64)
        .sum();

    let visited = AtomicBitset::new(n);
    let mut frontier: Vec<GlobalId> = q
        .starts
        .iter()
        .copied()
        .filter(|&s| visited.test_and_set(s.index()))
        .collect();
    let mut reached = Vec::new();
    let mut per_step = Vec::new();
    let mut explored = 0u64;
    let mut pulling = false;
    let mut pull_steps = 0;
    let mut capped = false;
    let mut scanned = 0u64;

    for _ in 0..q.max_steps {
        if frontier.is_empty() {
            break;
        }
        let frontier_links = degree_sum(view, &out_tables, &frontier);
        explored += frontier_links;
        if !pulling && frontier_links > total_links.saturating_sub(explored) / ALPHA {
            pulling = true;
        } else if pulling && (frontier.len() as u64) < n as u64 / BETA {
            pulling = false;
        }
        let next = if pulling {
            pull_steps += 1;
            let (next, checked) = pull_step(view, &in_tables, q.through, &visited, &frontier);
            scanned += checked;
            next
        } else {
            scanned += frontier_links;
            push_step(view, &out_tables, q.through, &visited, &frontier)
        };
        per_step.push(next.len() as u64);
        reached.extend_from_slice(&next);
        if let Some(cap) = q.cap
            && reached.len() as u64 >= cap
        {
            capped = true;
            break;
        }
        frontier = next;
    }
    ReachResult {
        reached,
        per_step,
        capped,
        pull_steps,
        scanned,
    }
}

/// Every node one step from `q.starts` (each once), stepping only where `q.through` allows. Unlike `reach`, the
/// starts aren't marked first: a start one step from another start is in the result.
pub fn first_step(view: &View, q: &Reach) -> Vec<GlobalId> {
    let tables = view.tables_by_type(q.link_types, q.direction);
    let visited = AtomicBitset::new(view.node_count());

    push_step(view, &tables, q.through, &visited, q.starts)
}


/// Push: go through the current step and mark each node's neighbours.
fn push_step(
    view: &View,
    tables: &IndexVec<node_type::Id, Vec<TableRef>>,
    through: Option<&Through>,
    visited: &AtomicBitset,
    frontier: &[GlobalId],
) -> Vec<GlobalId> {
    let expand = |u: GlobalId, out: &mut Vec<GlobalId>| {
        for table in &tables[view.type_of(u)] {
            view.for_each(table, u, |v| {
                if visited.get(v.index()) {
                    return;
                }
                if view.is_deleted(v)
                    || through.is_some_and(|t| !t.allows(view, table.other_type, v))
                {
                    visited.test_and_set(v.index()); // never allowed: don't look at it again
                    return;
                }
                if visited.test_and_set(v.index()) {
                    out.push(v);
                }
            });
        }
    };
    if frontier.len() < PARALLEL_MIN || rayon::current_num_threads() == 1 {
        let mut out = Vec::new();
        for &u in frontier {
            expand(u, &mut out);
        }
        out
    } else {
        frontier
            .par_chunks(CHUNK)
            .fold(Vec::new, |mut out, chunk| {
                for &u in chunk {
                    expand(u, &mut out);
                }
                out
            })
            .reduce(Vec::new, |mut a, mut b| {
                a.append(&mut b);
                a
            })
    }
}

/// Pull: go through the nodes not visited yet; each asks whether any node pointing to it is in the current step.
/// Split by ranges of node numbers, so each thread only marks its own bits.
fn pull_step(
    view: &View,
    in_tables: &IndexVec<node_type::Id, Vec<TableRef>>,
    through: Option<&Through>,
    visited: &AtomicBitset,
    frontier: &[GlobalId],
) -> (Vec<GlobalId>, u64) {
    let n = visited.len();
    let mut current = Bitset::new(n);
    for &u in frontier {
        current.set(u.index());
    }
    let scan_word = |w: usize, out: &mut Vec<GlobalId>, checked: &mut u64| {
        let mut free = !visited.word(w);
        let end = n - w * 64;
        if end < 64 {
            free &= (1u64 << end) - 1;
        }
        while free != 0 {
            let i = w * 64 + free.trailing_zeros() as usize;
            free &= free - 1;
            let g = GlobalId(i as u64);
            let ty = view.type_of(g);
            let tables = &in_tables[ty];
            if tables.is_empty() || view.is_deleted(g) {
                continue;
            }
            if let Some(t) = through
                && !t.allows(view, ty, g)
            {
                continue;
            }
            for table in tables {
                if view.any(table, g, |u| current.get(u.index()), checked) {
                    if visited.test_and_set(i) {
                        out.push(g);
                    }
                    break;
                }
            }
        }
    };
    let words = visited.word_count();
    if rayon::current_num_threads() == 1 {
        let mut out = Vec::new();
        let mut checked = 0;
        for w in 0..words {
            scan_word(w, &mut out, &mut checked);
        }
        (out, checked)
    } else {
        (0..words)
            .into_par_iter()
            .with_min_len(64)
            .fold(
                || (Vec::new(), 0u64),
                |(mut out, mut checked), w| {
                    scan_word(w, &mut out, &mut checked);
                    (out, checked)
                },
            )
            .reduce(
                || (Vec::new(), 0),
                |(mut a, x), (mut b, y)| {
                    a.append(&mut b);
                    (a, x + y)
                },
            )
    }
}

/// Per-thread distance arrays for two-sided walks, reset through the list of entries touched, so a small walk
/// doesn't pay for clearing the whole graph.
struct Scratch {
    forward: Vec<u32>,
    backward: Vec<u32>,
    touched_forward: Vec<GlobalId>,
    touched_backward: Vec<GlobalId>,
}

thread_local! {
    static SCRATCH: RefCell<Scratch> = const { RefCell::new(Scratch { forward: Vec::new(), backward: Vec::new(), touched_forward: Vec::new(), touched_backward: Vec::new() }) };
}

const UNSEEN: u32 = u32::MAX;

/// Length of the shortest path from `from` to `to` (or of the shortest loop through `from` when they are equal),
/// walking from both ends and always expanding the side whose current step is smaller.
pub fn path_length(
    view: &View,
    from: GlobalId,
    to: GlobalId,
    link_types: &[link_type::Id],
    direction: Direction,
    max_steps: u32,
    through: Option<&Through>,
) -> Option<u32> {
    let n = view.node_count();
    let forward_tables = view.tables_by_type(link_types, direction);
    let backward_tables = view.tables_by_type(link_types, reverse(direction));
    SCRATCH.with(|cell| {
        let mut guard = cell.borrow_mut();
        let s = &mut *guard;
        if s.forward.len() < n {
            s.forward.resize(n, UNSEEN);
            s.backward.resize(n, UNSEEN);
        }
        s.forward[from.index()] = 0;
        s.touched_forward.push(from);
        s.backward[to.index()] = 0;
        s.touched_backward.push(to);

        let mut front_f = vec![from];
        let mut front_b = vec![to];
        let (mut level_f, mut level_b) = (0u32, 0u32);
        let mut best = UNSEEN;
        let mut next = Vec::new();
        loop {
            if level_f + level_b >= max_steps || (best != UNSEEN && best <= level_f + level_b + 1) {
                break;
            }
            if front_f.is_empty() || front_b.is_empty() {
                break;
            }
            next.clear();
            if front_f.len() <= front_b.len() {
                expand(
                    view,
                    &forward_tables,
                    through,
                    &front_f,
                    level_f,
                    &mut s.forward,
                    &mut s.touched_forward,
                    &s.backward,
                    &mut best,
                    &mut next,
                );
                level_f += 1;
                std::mem::swap(&mut front_f, &mut next);
            } else {
                expand(
                    view,
                    &backward_tables,
                    through,
                    &front_b,
                    level_b,
                    &mut s.backward,
                    &mut s.touched_backward,
                    &s.forward,
                    &mut best,
                    &mut next,
                );
                level_b += 1;
                std::mem::swap(&mut front_b, &mut next);
            }
        }
        for &g in &s.touched_forward {
            s.forward[g.index()] = UNSEEN;
        }
        for &g in &s.touched_backward {
            s.backward[g.index()] = UNSEEN;
        }
        s.touched_forward.clear();
        s.touched_backward.clear();
        (best != UNSEEN && best <= max_steps).then_some(best)
    })
}

#[allow(clippy::too_many_arguments)]
fn expand(
    view: &View,
    tables: &IndexVec<node_type::Id, Vec<TableRef>>,
    through: Option<&Through>,
    front: &[GlobalId],
    level: u32,
    mine: &mut [u32],
    touched: &mut Vec<GlobalId>,
    other: &[u32],
    best: &mut u32,
    next: &mut Vec<GlobalId>,
) {
    for &u in front {
        for table in &tables[view.type_of(u)] {
            view.for_each(table, u, |v| {
                let vi = v.index();
                let theirs = other[vi];
                if theirs != 0 && view.is_deleted(v) {
                    return;
                }
                if let Some(t) = through {
                    // The other side's own end is always allowed; any other node must meet the condition.
                    if theirs != 0 && !t.allows(view, table.other_type, v) {
                        return;
                    }
                }
                if theirs != UNSEEN {
                    *best = (*best).min(level + 1 + theirs);
                }
                if mine[vi] == UNSEEN {
                    mine[vi] = level + 1;
                    touched.push(v);
                    next.push(v);
                }
            });
        }
    }
}
