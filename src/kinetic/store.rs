//! Generations: the packed graph plus its edits, rebuilt when the edits grow past a share of the packed data.
//!
//! - A search takes the current generation (`Store::current`) and a view of it. A generation stays alive as long as
//!   a search still uses it.
//! - The single writer journals every action. When the edits since the last rebuild pass `share` of the packed
//!   links (links added or removed), or of the packed nodes (nodes added or deleted, property changes), it starts a
//!   rebuild on another thread from the packed graph plus the journal so far. Writes continue meanwhile.
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
use crate::kinetic::live::{Generation, Live};
use crate::kinetic::rebuild::{self, NONE, Op, Rebuilt};
use crate::storage::graph::Graph;
use crate::storage::link_type;
use crate::storage::node_type;

/// By default, rebuild when the edits pass this share of the packed links or nodes …
const DEFAULT_SHARE: f64 = 0.05;

/// … but never for fewer edits than this.
const DEFAULT_MIN_EDITS: u64 = 1_000;

/// By default, the rebuild thread hands over to the writer once a replay round finds fewer edits than this.
const DEFAULT_CATCH_UP: usize = 1_000;

/// … or after this many rounds, if writes arrive faster than they can be replayed.
const MAX_ROUNDS: usize = 50;

/// Journaled actions: (sequence number, edit).
type Ops = Vec<(u64, Op)>;


pub struct Store {
    current: RwLock<Arc<Generation>>,
    /// The writer's state (journal, running rebuild) while no writer is out.
    parked: Mutex<Option<State>>,
    /// Rebuild when the edits pass this share of the packed links or nodes …
    share: f64,
    /// … but never for fewer edits than this.
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


impl Store {
    pub fn new(graph: Graph) -> Self {
        Self::with_limits(graph, DEFAULT_SHARE, DEFAULT_MIN_EDITS)
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
            catch_up: DEFAULT_CATCH_UP,
        }
    }


    /// Sets when background replay hands over to the writer. Tests set it low to make replay rounds race the writer.
    pub fn with_catch_up(mut self, edits: usize) -> Self {
        self.catch_up = edits.max(1);
        self
    }


    /// The generation new searches should use.
    pub fn current(&self) -> Arc<Generation> {
        self.current
            .read()
            .expect("no writer panics holding the lock")
            .clone()
    }


    /// The single writer.
    pub fn writer(&self) -> Writer<'_> {
        let state = self
            .parked
            .lock()
            .expect("no writer panics holding the lock")
            .take()
            .expect("only one writer at a time");

        Writer {
            store: self,
            state: ManuallyDrop::new(state),
        }
    }


    pub fn rebuilds(&self) -> u64 {
        self.rebuilds.load(Ordering::Acquire)
    }
}


// ---------------------------------------------------------------- the writer


pub struct Writer<'s> {
    store: &'s Store,
    state: ManuallyDrop<State>,
}


struct State {
    editor: Editor,
    /// The actions since this generation's packed graph was built (or since the running rebuild started).
    journal: Ops,
    link_edits: u64,
    node_edits: u64,
    link_limit: u64,
    node_limit: u64,
    building: Option<Building>,
    finished: Vec<RebuildStats>,
}


impl Drop for Writer<'_> {
    fn drop(&mut self) {
        // Safety: drop runs once; `state` is not used after this.
        let state = unsafe { ManuallyDrop::take(&mut self.state) };

        *self
            .store
            .parked
            .lock()
            .expect("no writer panics holding the lock") = Some(state);
    }
}


impl Writer<'_> {
    /// The generation the writer edits; node numbers given to the writer are this generation's.
    pub fn generation(&self) -> &Arc<Generation> {
        self.state.editor.generation()
    }


    pub fn begin(&mut self, seq: u64) {
        self.state.editor.begin(seq);
    }


    /// Publishes the action. Between actions, swaps in a finished rebuild or starts one when the edits got big.
    /// After a swap, node numbers from the old generation are no longer valid: look nodes up again by key.
    pub fn commit(&mut self) {
        self.state.editor.commit();
        self.check();
    }


    /// Call when no actions are waiting (for example every few milliseconds while the queue is empty): swaps in a
    /// finished rebuild, or starts one if the edits are past the limit, the same check `commit` makes.
    pub fn idle(&mut self) {
        self.check();
    }


    fn check(&mut self) {
        match &self.state.building {
            Some(building) => {
                if building.handle.is_finished() {
                    self.swap();
                }
            }

            None => {
                let past_limit = self.state.link_edits > self.state.link_limit
                    || self.state.node_edits > self.state.node_limit;

                if past_limit {
                    self.start_rebuild();
                }
            }
        }
    }


    /// Starts a rebuild now if none is running (between actions only).
    pub fn rebuild_now(&mut self) {
        if self.state.building.is_none() {
            self.start_rebuild();
        }
    }


    /// Waits for a running rebuild and swaps it in (between actions only).
    pub fn finish_rebuild(&mut self) {
        if self.state.building.is_some() {
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
        self.state.building.is_some()
    }


    /// Costs of the rebuilds finished so far, oldest first.
    pub fn rebuild_stats(&self) -> &[RebuildStats] {
        &self.state.finished
    }


    pub fn add_node(&mut self, node_type: node_type::Id, key: &[u8]) -> node::GlobalId {
        let node = self.state.editor.add_node(node_type, key);

        self.record(Op::AddNode {
            node_type,
            key: key.to_vec(),
        });
        self.state.node_edits += 1;

        node
    }


    pub fn set_property(&mut self, node: node::GlobalId, name: &str, value: &Value) {
        let (node_type, _) = self.state.editor.get(node);
        let property = self.state.editor.property_index(node_type, name);

        self.state.editor.set_property(node, property, value);

        self.record(Op::SetProperty {
            node,
            property,
            value: value.clone(),
        });
        self.state.node_edits += 1;
    }


    pub fn delete_node(&mut self, node: node::GlobalId) {
        self.state.editor.delete_node(node);

        self.record(Op::DeleteNode { node });
        self.state.node_edits += 1;
    }


    pub fn add_link(
        &mut self,
        link_type: link_type::Id,
        from: node::GlobalId,
        to: node::GlobalId,
    ) -> link::Id {
        let link = self.state.editor.add_link(link_type, from, to);

        self.record(Op::AddLink {
            link_type,
            from,
            to,
        });
        self.state.link_edits += 1;

        link
    }


    /// Removes one link `from → to` of `link_type`; false if there is none.
    pub fn remove_link(
        &mut self,
        link_type: link_type::Id,
        from: node::GlobalId,
        to: node::GlobalId,
    ) -> bool {
        let found = self.state.editor.remove_link(link_type, from, to);

        if found {
            self.record(Op::RemoveLink {
                link_type,
                from,
                to,
            });
            self.state.link_edits += 1;
        }

        found
    }


    /// Journals an edit of the current action: into the generation's journal, or into the backlog while a rebuild
    /// runs.
    fn record(&mut self, op: Op) {
        let seq = self.state.editor.seq();

        match &self.state.building {
            Some(building) => building
                .backlog
                .lock()
                .expect("no replay round panics holding the lock")
                .push((seq, op)),

            None => self.state.journal.push((seq, op)),
        }
    }


    fn start_rebuild(&mut self) {
        let ops = std::mem::take(&mut self.state.journal);
        let old = self.state.editor.generation().clone();
        // The journal ends here: the new generation starts as of this action.
        let seq = old.live.published();
        let backlog: Arc<Mutex<Ops>> = Arc::default();

        let thread_backlog = backlog.clone();
        let catch_up = self.store.catch_up;

        let handle =
            std::thread::spawn(move || build_and_catch_up(old, ops, seq, thread_backlog, catch_up));

        self.state.building = Some(Building { handle, backlog });
    }


    /// Replays what is left of the backlog onto the new generation, then makes it current.
    fn swap(&mut self) {
        let building = self
            .state
            .building
            .take()
            .expect("swap is only called while a rebuild runs");

        let CaughtUp {
            mut replay,
            build,
            catch_up,
            rounds,
        } = building
            .handle
            .join()
            .expect("the rebuild thread doesn't panic");

        let paused_at = Instant::now();
        let replayed_behind = replay.actions;

        let rest = std::mem::take(
            &mut *building
                .backlog
                .lock()
                .expect("no replay round panics holding the lock"),
        );

        replay.apply(rest);
        replay.finish();

        *self
            .store
            .current
            .write()
            .expect("no writer panics holding the lock") = replay.editor.generation().clone();

        let replayed_paused = replay.actions - replayed_behind;

        self.state.editor = replay.editor;
        self.state.journal = replay.journal;
        self.state.link_edits = replay.link_edits;
        self.state.node_edits = replay.node_edits;
        self.state
            .set_limits(self.store.share, self.store.min_edits);

        self.store.rebuilds.fetch_add(1, Ordering::AcqRel);

        self.state.finished.push(RebuildStats {
            build,
            replayed_behind,
            rounds,
            catch_up,
            replayed_paused,
            pause: paused_at.elapsed(),
        });
    }
}


impl State {
    fn set_limits(&mut self, share: f64, min_edits: u64) {
        let graph = &self.editor.generation().graph;

        let links: u64 = graph
            .link_types
            .iter()
            .map(|storage| storage.forward.len() as u64)
            .sum();

        let limit = |packed: u64| ((packed as f64 * share) as u64).max(min_edits);

        self.link_limit = limit(links);
        self.node_limit = limit(graph.node_count() as u64);
    }
}


// ---------------------------------------------------------------- the rebuild thread


struct Building {
    handle: JoinHandle<CaughtUp>,
    /// Actions written since the rebuild started, not yet taken by a replay round.
    backlog: Arc<Mutex<Ops>>,
}


struct CaughtUp {
    replay: Replay,
    build: Duration,
    catch_up: Duration,
    rounds: usize,
}


/// The rebuild thread: builds the new packed graph, then replays the backlog onto it in rounds until a round is
/// small (or `MAX_ROUNDS` ran). The writer replays what is left when it swaps.
fn build_and_catch_up(
    old: Arc<Generation>,
    ops: Ops,
    seq: u64,
    backlog: Arc<Mutex<Ops>>,
    catch_up: usize,
) -> CaughtUp {
    let build_started = Instant::now();
    let Rebuilt { graph, remap } = rebuild::rebuild(&old.graph, &ops);
    drop(ops);
    let build = build_started.elapsed();

    let catch_up_started = Instant::now();

    let generation = Arc::new(Generation {
        live: Live::starting_at(&graph, seq),
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
        let round = std::mem::take(
            &mut *backlog
                .lock()
                .expect("the writer doesn't panic holding the lock"),
        );
        let size = round.len();

        if size > 0 {
            rounds += 1;
            replay.apply(round);
        }

        if size < catch_up || rounds >= MAX_ROUNDS {
            break;
        }
    }

    CaughtUp {
        replay,
        build,
        catch_up: catch_up_started.elapsed(),
        rounds,
    }
}


/// Applies journaled actions of the old generation to the new one, translating node numbers.
struct Replay {
    editor: Editor,
    /// Old node number → new one (or `NONE`); grows as replayed actions add nodes.
    remap: Vec<node::GlobalId>,
    /// The new generation's journal: the replayed actions with new node numbers.
    journal: Ops,
    link_edits: u64,
    node_edits: u64,
    /// The action being replayed; an action may be split across rounds, so it is published when the next begins.
    open: Option<u64>,
    actions: usize,
}


impl Replay {
    fn apply(&mut self, ops: Ops) {
        for (seq, op) in ops {
            if self.open != Some(seq) {
                self.start_action(seq);
            }

            self.replay_one(seq, op);
        }
    }


    /// Publishes the action before `seq` (it is complete) and begins `seq`.
    fn start_action(&mut self, seq: u64) {
        if self.open.is_some() {
            self.editor.commit();
        }

        self.editor.begin(seq);
        self.open = Some(seq);
        self.actions += 1;
    }


    /// The new number of an old node, unless it was deleted before the rebuild.
    fn new_number(&self, old: node::GlobalId) -> Option<node::GlobalId> {
        self.remap
            .get(old.index())
            .copied()
            .filter(|&new| new != NONE)
    }


    fn replay_one(&mut self, seq: u64, op: Op) {
        match op {
            Op::AddNode { node_type, key } => {
                let node = self.editor.add_node(node_type, &key);

                self.remap.push(node);
                self.journal.push((seq, Op::AddNode { node_type, key }));
                self.node_edits += 1;
            }

            Op::SetProperty {
                node,
                property,
                value,
            } => {
                let Some(node) = self.new_number(node) else {
                    return;
                };

                self.editor.set_property(node, property, &value);

                self.journal.push((
                    seq,
                    Op::SetProperty {
                        node,
                        property,
                        value,
                    },
                ));
                self.node_edits += 1;
            }

            Op::DeleteNode { node } => {
                let Some(node) = self.new_number(node) else {
                    return;
                };

                self.editor.delete_node(node);

                self.journal.push((seq, Op::DeleteNode { node }));
                self.node_edits += 1;
            }

            Op::AddLink {
                link_type,
                from,
                to,
            } => {
                let (Some(from), Some(to)) = (self.new_number(from), self.new_number(to)) else {
                    return;
                };

                self.editor.add_link(link_type, from, to);

                self.journal.push((
                    seq,
                    Op::AddLink {
                        link_type,
                        from,
                        to,
                    },
                ));
                self.link_edits += 1;
            }

            Op::RemoveLink {
                link_type,
                from,
                to,
            } => {
                let (Some(from), Some(to)) = (self.new_number(from), self.new_number(to)) else {
                    return;
                };

                if !self.editor.remove_link(link_type, from, to) {
                    return;
                }

                self.journal.push((
                    seq,
                    Op::RemoveLink {
                        link_type,
                        from,
                        to,
                    },
                ));
                self.link_edits += 1;
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
