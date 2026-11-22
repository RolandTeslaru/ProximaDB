//! Generations: the packed graph plus its edits, rebuilt when the edits grow past a share of the packed data.
//!
//! - A search takes the current generation (`Store::current`) and a view of it. A generation stays alive as long as
//!   a search still uses it.
//! - The single writer journals every action. When the edits since the last rebuild pass `share` of the packed
//!   connections (links added or removed), or of the packed nodes (nodes added or deleted, property changes), it
//!   starts a rebuild on another thread from the packed graph plus the journal so far. Writes continue meanwhile.
//! - Actions that arrive during the build go to a shared backlog. When the build is done, the rebuild thread replays
//!   the backlog onto the new generation in rounds, each round taking what arrived during the previous one, until
//!   what is left is small. Then the writer pauses, replays that last part itself, and swaps the new generation in.
//!   Searches already running finish on the old one.

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::core::Value;
use crate::core::link;
use crate::core::node;
use crate::kinetic::editor::Editor;
use crate::kinetic::live::Generation;
use crate::kinetic::rebuild::{self, NONE, Op, Rebuilt};
use crate::storage::graph::Graph;
use crate::storage::link_type;
use crate::storage::node_type;

pub struct Store {
    current: RwLock<Arc<Generation>>,
    /// The writer's state (journal, running rebuild) while no writer is out.
    parked: Mutex<Option<State>>,
    /// Rebuild when the edits pass this share of the packed connections or nodes.
    share: f64,
    /// ... but never for fewer edits than this.
    min_edits: u64,
    rebuilds: AtomicU64,
    /// The rebuild thread stops replaying once a round finds fewer edits than this; the writer does the rest.
    catch_up: usize,
}

/// What one finished rebuild cost.
#[derive(Clone, Copy, Debug)]
pub struct RebuildStats {
    /// Building the new packed graph, on its own thread.
    pub build: Duration,
    /// Actions replayed by the rebuild thread while the writer kept writing, and in how many rounds.
    pub replayed_behind: usize,
    pub rounds: usize,
    /// Replaying in the background.
    pub catch_up: Duration,
    /// Actions left at the end, replayed by the writer before the swap.
    pub replayed_paused: usize,
    /// How long the writer was paused for them and the swap.
    pub pause: Duration,
}

/// ... or after this many rounds, if writes arrive faster than they can be replayed.
const MAX_ROUNDS: usize = 50;

impl Store {
    /// Rebuilds at 5% of the packed connections or nodes, and at no fewer than 1,000 edits.
    pub fn new(graph: Graph) -> Self {
        Self::with_limits(graph, 0.05, 1_000)
    }

    pub fn with_limits(graph: Graph, share: f64, min_edits: u64) -> Self {
        let current = Arc::new(Generation::new(graph));
        let mut state = State {
            editor: Editor::new(current.clone()),
            journal: Vec::new(),
            link_edits: 0,
            node_edits: 0,
            link_limit: 0,
            node_limit: 0,
            building: None,
            finished: Vec::new(),
        };
        state.set_limits(share, min_edits);
        Self {
            current: RwLock::new(current),
            parked: Mutex::new(Some(state)),
            share,
            min_edits,
            rebuilds: AtomicU64::new(0),
            catch_up: 1_000,
        }
    }

    /// Sets when background replay hands over to the writer (default 1,000 edits). Tests set it low to make
    /// replay rounds race the writer.
    pub fn with_catch_up(mut self, edits: usize) -> Self {
        self.catch_up = edits.max(1);
        self
    }

    /// The generation new searches should use.
    pub fn current(&self) -> Arc<Generation> {
        self.current.read().unwrap().clone()
    }

    /// The single writer.
    pub fn writer(&self) -> Writer<'_> {
        let state = self
            .parked
            .lock()
            .unwrap()
            .take()
            .expect("only one writer at a time");
        Writer {
            store: self,
            s: ManuallyDrop::new(state),
        }
    }

    pub fn rebuilds(&self) -> u64 {
        self.rebuilds.load(Ordering::Acquire)
    }
}

struct Building {
    handle: JoinHandle<CaughtUp>,
    /// Actions written since the rebuild started, not yet taken by a replay round.
    backlog: Arc<Mutex<Vec<(u64, Op)>>>,
}

struct CaughtUp {
    replay: Replay,
    build: Duration,
    catch_up: Duration,
    rounds: usize,
}

/// Applies journaled actions of the old generation to the new one, translating node numbers.
struct Replay {
    editor: Editor,
    /// Old node number → new one (or `NONE`); grows as replayed actions add nodes.
    remap: Vec<node::GlobalId>,
    /// The new generation's journal: the replayed actions with new node numbers.
    journal: Vec<(u64, Op)>,
    link_edits: u64,
    node_edits: u64,
    /// The action being replayed; an action may be split across rounds, so it is published when the next begins.
    open: Option<u64>,
    actions: usize,
}

impl Replay {
    fn apply(&mut self, ops: Vec<(u64, Op)>) {
        for (seq, op) in ops {
            if self.open != Some(seq) {
                if self.open.is_some() {
                    self.editor.commit();
                }
                self.editor.begin(seq);
                self.open = Some(seq);
                self.actions += 1;
            }
            let remap = &self.remap;
            let new = |g: node::GlobalId| remap.get(g.index()).copied().filter(|&n| n != NONE);
            let editor = &mut self.editor;
            match op {
                Op::AddNode { t, key } => {
                    let g = editor.add_node(t, &key);
                    self.remap.push(g);
                    self.journal.push((seq, Op::AddNode { t, key }));
                    self.node_edits += 1;
                }
                Op::SetProperty {
                    node,
                    property,
                    value,
                } => {
                    if let Some(n) = new(node) {
                        editor.set_property(n, property, &value);
                        self.journal.push((
                            seq,
                            Op::SetProperty {
                                node: n,
                                property,
                                value,
                            },
                        ));
                        self.node_edits += 1;
                    }
                }
                Op::DeleteNode { node } => {
                    if let Some(n) = new(node) {
                        editor.delete_node(n);
                        self.journal.push((seq, Op::DeleteNode { node: n }));
                        self.node_edits += 1;
                    }
                }
                Op::AddLink { r, from, to } => {
                    if let (Some(a), Some(c)) = (new(from), new(to)) {
                        editor.add_link(r, a, c);
                        self.journal.push((seq, Op::AddLink { r, from: a, to: c }));
                        self.link_edits += 1;
                    }
                }
                Op::RemoveLink { r, from, to } => {
                    if let (Some(a), Some(c)) = (new(from), new(to)) {
                        if editor.remove_link(r, a, c) {
                            self.journal
                                .push((seq, Op::RemoveLink { r, from: a, to: c }));
                            self.link_edits += 1;
                        }
                    }
                }
            }
        }
    }

    /// Publishes the last action (called by the writer, between actions, so it is complete).
    fn finish(&mut self) {
        if self.open.take().is_some() {
            self.editor.commit();
        }
    }
}

pub struct Writer<'s> {
    store: &'s Store,
    s: ManuallyDrop<State>,
}

struct State {
    editor: Editor,
    /// The actions since this generation's packed graph was built (or since the running rebuild started).
    journal: Vec<(u64, Op)>,
    link_edits: u64,
    node_edits: u64,
    link_limit: u64,
    node_limit: u64,
    building: Option<Building>,
    finished: Vec<RebuildStats>,
}

impl Drop for Writer<'_> {
    fn drop(&mut self) {
        // Safety: drop runs once; `s` is not used after this.
        let state = unsafe { ManuallyDrop::take(&mut self.s) };
        *self.store.parked.lock().unwrap() = Some(state);
    }
}

impl Writer<'_> {
    /// The generation the writer edits; node numbers given to the writer are this generation's.
    pub fn generation(&self) -> &Arc<Generation> {
        self.s.editor.generation()
    }

    pub fn begin(&mut self, seq: u64) {
        self.s.editor.begin(seq);
    }

    /// Publishes the action. Between actions, swaps in a finished rebuild or starts one when the edits got big.
    /// After a swap, node numbers from the old generation are no longer valid: look nodes up again by key.
    pub fn commit(&mut self) {
        self.s.editor.commit();
        self.check();
    }

    /// Call when no actions are waiting (for example every few milliseconds while the queue is empty): swaps in a
    /// finished rebuild, or starts one if the edits are past the limit, the same check `commit` makes.
    pub fn idle(&mut self) {
        self.check();
    }

    fn check(&mut self) {
        if let Some(b) = &self.s.building {
            if b.handle.is_finished() {
                self.swap();
            }
        } else if self.s.link_edits > self.s.link_limit || self.s.node_edits > self.s.node_limit {
            self.start_rebuild();
        }
    }

    /// Starts a rebuild now if none is running (between actions only).
    pub fn rebuild_now(&mut self) {
        if self.s.building.is_none() {
            self.start_rebuild();
        }
    }

    /// Waits for a running rebuild and swaps it in (between actions only).
    pub fn finish_rebuild(&mut self) {
        if self.s.building.is_some() {
            self.swap();
        }
    }

    /// Folds every action so far into a new packed graph and waits for it (between actions only).
    pub fn rebuild_and_wait(&mut self) {
        self.finish_rebuild();
        self.start_rebuild();
        self.swap();
    }

    pub fn is_rebuilding(&self) -> bool {
        self.s.building.is_some()
    }

    /// Costs of the rebuilds finished so far, oldest first.
    pub fn rebuild_stats(&self) -> &[RebuildStats] {
        &self.s.finished
    }

    fn seq(&self) -> u64 {
        self.s.editor.seq()
    }

    pub fn add_node(&mut self, t: node_type::Id, key: &[u8]) -> node::GlobalId {
        let g = self.s.editor.add_node(t, key);
        let seq = self.seq();
        self.record(
            seq,
            Op::AddNode {
                t,
                key: key.to_vec(),
            },
        );
        self.s.node_edits += 1;
        g
    }

    pub fn set_property(&mut self, g: node::GlobalId, name: &str, value: &Value) {
        let (t, _) = self.s.editor.get(g);
        let property = self.s.editor.property_index(t, name);
        self.s.editor.set_property(g, property, value);
        let seq = self.seq();
        self.record(
            seq,
            Op::SetProperty {
                node: g,
                property,
                value: value.clone(),
            },
        );
        self.s.node_edits += 1;
    }

    pub fn delete_node(&mut self, g: node::GlobalId) {
        self.s.editor.delete_node(g);
        let seq = self.seq();
        self.record(seq, Op::DeleteNode { node: g });
        self.s.node_edits += 1;
    }

    pub fn add_link(
        &mut self,
        r: link_type::Id,
        from: node::GlobalId,
        to: node::GlobalId,
    ) -> link::Id {
        let e = self.s.editor.add_link(r, from, to);
        let seq = self.seq();
        self.record(
            seq,
            Op::AddLink {
                r,
                from,
                to,
            },
        );
        self.s.link_edits += 1;
        e
    }

    /// Removes one link `from → to` of link type `r`; false if there is none.
    pub fn remove_link(
        &mut self,
        r: link_type::Id,
        from: node::GlobalId,
        to: node::GlobalId,
    ) -> bool {
        let found = self.s.editor.remove_link(r, from, to);
        if found {
            let seq = self.seq();
            self.record(
                seq,
                Op::RemoveLink {
                    r,
                    from,
                    to,
                },
            );
            self.s.link_edits += 1;
        }
        found
    }

    /// Journals an edit: into the generation's journal, or into the backlog while a rebuild runs.
    fn record(&mut self, seq: u64, op: Op) {
        match &self.s.building {
            Some(b) => b.backlog.lock().unwrap().push((seq, op)),
            None => self.s.journal.push((seq, op)),
        }
    }

    fn start_rebuild(&mut self) {
        let ops = std::mem::take(&mut self.s.journal);
        let old = self.s.editor.generation().clone();
        let seq = old.live.published();
        let backlog: Arc<Mutex<Vec<(u64, Op)>>> = Arc::default();
        let theirs = backlog.clone();
        let catch_up = self.store.catch_up;
        let handle = std::thread::spawn(move || {
            let t = Instant::now();
            let Rebuilt { graph, remap } = rebuild::rebuild(&old.graph, &ops);
            drop(ops);
            let build = t.elapsed();
            let t = Instant::now();
            let generation = Arc::new(Generation {
                live: crate::kinetic::live::Live::starting_at(&graph, seq),
                graph,
            });
            let mut replay = Replay {
                editor: Editor::new(generation),
                remap,
                journal: Vec::new(),
                link_edits: 0,
                node_edits: 0,
                open: None,
                actions: 0,
            };
            let mut rounds = 0;
            loop {
                let batch = std::mem::take(&mut *theirs.lock().unwrap());
                let n = batch.len();
                if n > 0 {
                    rounds += 1;
                    replay.apply(batch);
                }
                if n < catch_up || rounds >= MAX_ROUNDS {
                    break;
                }
            }
            CaughtUp {
                replay,
                build,
                catch_up: t.elapsed(),
                rounds,
            }
        });
        self.s.building = Some(Building { handle, backlog });
    }

    /// Replays what is left of the backlog onto the new generation, then makes it current.
    fn swap(&mut self) {
        let b = self.s.building.take().unwrap();
        let CaughtUp {
            mut replay,
            build,
            catch_up,
            rounds,
        } = b.handle.join().expect("rebuild");
        let started = Instant::now();
        let behind = replay.actions;
        let rest = std::mem::take(&mut *b.backlog.lock().unwrap());
        replay.apply(rest);
        replay.finish();
        let paused_actions = replay.actions - behind;
        *self.store.current.write().unwrap() = replay.editor.generation().clone();
        self.s.editor = replay.editor;
        self.s.journal = replay.journal;
        self.s.link_edits = replay.link_edits;
        self.s.node_edits = replay.node_edits;
        self.s.set_limits(self.store.share, self.store.min_edits);
        self.store.rebuilds.fetch_add(1, Ordering::AcqRel);
        self.s.finished.push(RebuildStats {
            build,
            replayed_behind: behind,
            rounds,
            catch_up,
            replayed_paused: paused_actions,
            pause: started.elapsed(),
        });
    }
}

impl State {
    fn set_limits(&mut self, share: f64, min_edits: u64) {
        let g = &self.editor.generation().graph;
        let links: u64 = g.link_types.iter().map(|r| r.forward.len() as u64).sum();
        let limit = |n: u64| ((n as f64 * share) as u64).max(min_edits);
        self.link_limit = limit(links);
        self.node_limit = limit(g.node_count() as u64);
    }
}
