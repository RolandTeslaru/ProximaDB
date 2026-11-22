//! Writes: actions go through a bounded queue to the one writer thread, which applies each whole and publishes it.
//!
//! The writer thread, for each action in order:
//!
//!   1. Rejects it if its sequence number isn't after the last published one.
//!   2. Checks every edit against the current generation (`check`); a bad edit rejects the whole action.
//!   3. Applies the edits, publishes the action at once, and tells the readers waiting on it.
//!
//! With no action for `IDLE`, it runs the writer's idle check, which may start or finish a rebuild.
//!
//! It keeps the last `KEPT_ACTIONS` actions it applied, as they arrived (by key, not node number), so that a graph
//! reloaded from Quarry's datasets as of some action can take every later one again (`reload`).

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use tokio::sync::{mpsc, oneshot, watch};

use proximadb::Store;
use proximadb::Writer;
use proximadb::core::node::GlobalId;
use proximadb::storage::node_type;

use crate::api::{ActionRequest, ActionResponse, ApiError, ApiResult, bad};
use crate::check::{Checked, Target, check_action};
use crate::state::{Shared, Swapped, WriteJob};

/// Actions waiting for the writer; a full queue makes callers wait.
pub(crate) const WRITE_QUEUE: usize = 1024;

/// With no action for this long, the writer runs its idle check.
const IDLE: Duration = Duration::from_millis(50);

/// Actions kept for replay after a reload. A reload as of an older action than these is refused.
const KEPT_ACTIONS: usize = 1_000_000;


pub(crate) async fn action(
    State(state): State<Shared>,
    Json(action): Json<ActionRequest>,
) -> ApiResult<ActionResponse> {
    let writer_stopped = || ApiError(StatusCode::SERVICE_UNAVAILABLE, "writer stopped".into());

    let (reply, result) = oneshot::channel();

    state
        .writes
        .send(WriteJob::Action { action, reply })
        .await
        .map_err(|_| writer_stopped())?;

    match result.await {
        Ok(Ok(done)) => Ok(Json(done)),
        Ok(Err(message)) => Err(bad(message)),
        Err(_) => Err(writer_stopped()),
    }
}


pub(crate) fn writer_thread(
    store: &'static Store,
    mut queue: mpsc::Receiver<WriteJob>,
    published: watch::Sender<u64>,
) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("a single-threaded runtime always builds");

    let mut thread = WriterThread {
        writer: store.writer(),
        published,
        kept: VecDeque::new(),
        dropped_through: 0,
    };

    runtime.block_on(async {
        loop {
            let job = match tokio::time::timeout(IDLE, queue.recv()).await {
                Ok(Some(job)) => job,

                // Every sender is gone: the server is shutting down.
                Ok(None) => break,

                Err(_) => {
                    thread.writer.idle();
                    continue;
                }
            };

            match job {
                WriteJob::Action { action, reply } => {
                    let _ = reply.send(thread.action(action));
                }

                WriteJob::Swap {
                    graph,
                    as_of_seq,
                    reply,
                } => {
                    let _ = reply.send(thread.swap(*graph, as_of_seq));
                }
            }
        }
    });
}


struct WriterThread {
    writer: Writer<'static>,
    published: watch::Sender<u64>,
    /// The last actions applied, oldest first.
    kept: VecDeque<ActionRequest>,
    /// The newest action no longer kept (0 when none was dropped).
    dropped_through: u64,
}


impl WriterThread {
    fn action(&mut self, action: ActionRequest) -> Result<ActionResponse, String> {
        let done = apply(&mut self.writer, action.clone())?;

        let _ = self.published.send(action.seq);

        self.keep(action);

        Ok(done)
    }


    fn keep(&mut self, action: ActionRequest) {
        self.kept.push_back(action);

        if self.kept.len() > KEPT_ACTIONS {
            let dropped = self.kept.pop_front().expect("over the limit, so not empty");

            self.dropped_through = dropped.seq;
        }
    }


    /// Swaps in a graph reloaded as of `as_of_seq`: the kept actions after it are applied again first, then searches
    /// move to it.
    fn swap(
        &mut self,
        graph: proximadb::storage::Graph,
        as_of_seq: u64,
    ) -> Result<Swapped, String> {
        if as_of_seq < self.dropped_through {
            return Err(format!(
                "the reloaded data is as of action {as_of_seq}, but actions up to {} are no longer kept: reload \
                 from newer data",
                self.dropped_through
            ));
        }

        let before = self.writer.generation().live.published();

        self.writer.replace(graph, as_of_seq);

        let later: Vec<ActionRequest> = self
            .kept
            .iter()
            .filter(|action| action.seq > as_of_seq)
            .cloned()
            .collect();

        let mut failed = Vec::new();

        for action in &later {
            if let Err(reason) = apply(&mut self.writer, action.clone()) {
                failed.push(format!("action {}: {reason}", action.seq));
            }
        }

        // A replayed action that no longer applies is still done: never answer with an older sequence than before.
        self.writer.include_up_to(before.max(as_of_seq));
        self.writer.make_current();

        // The data holds the actions up to `as_of_seq`; the ones kept after it now sit on top.
        self.kept.retain(|action| action.seq > as_of_seq);
        self.dropped_through = self.dropped_through.max(as_of_seq);

        let seq = self.writer.generation().live.published();
        let _ = self.published.send(seq);

        Ok(Swapped {
            replayed: later.len(),
            failed,
            seq,
        })
    }
}


/// Checks and applies one action; nothing is applied unless all of it is valid.
fn apply(writer: &mut Writer, action: ActionRequest) -> Result<ActionResponse, String> {
    let last = writer.generation().live.published();

    if action.seq <= last {
        return Err(format!(
            "seq {} is not after the last published action {last}",
            action.seq
        ));
    }

    let edits = {
        let view = writer.generation().view();

        check_action(&view, &action.edits)?
    };

    writer.begin(action.seq);

    let mut added: HashMap<(node_type::Id, String), GlobalId> = HashMap::new();
    let mut missing_links = 0;

    for edit in edits {
        match edit {
            Checked::AddNode(node_type, key, properties) => {
                let node = writer.add_node(node_type, key.as_bytes());

                for (name, value) in &properties {
                    writer.set_property(node, name, value);
                }

                added.insert((node_type, key), node);
            }

            Checked::SetProperty(target, name, value) => {
                writer.set_property(resolve(&target, &added), &name, &value);
            }

            Checked::DeleteNode(target) => {
                writer.delete_node(resolve(&target, &added));
            }

            Checked::AddLink(link_type, from, to) => {
                writer.add_link(link_type, resolve(&from, &added), resolve(&to, &added));
            }

            Checked::RemoveLink(link_type, from, to) => {
                let removed =
                    writer.remove_link(link_type, resolve(&from, &added), resolve(&to, &added));

                if !removed {
                    missing_links += 1;
                }
            }
        }
    }

    writer.commit();

    Ok(ActionResponse {
        seq: action.seq,
        missing_links,
    })
}


/// The node a checked edit names: an existing one, or one an earlier edit of the same action added.
fn resolve(target: &Target, added: &HashMap<(node_type::Id, String), GlobalId>) -> GlobalId {
    match target {
        Target::Existing(node) => *node,
        Target::Added(node_type, key) => added[&(*node_type, key.clone())],
    }
}
