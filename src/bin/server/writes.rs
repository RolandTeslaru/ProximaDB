//! Writes: actions go through a bounded queue to the one writer thread, which applies each whole and publishes it.
//!
//! The writer thread, for each action in order:
//!
//!   1. Rejects it if its sequence number isn't after the last published one.
//!   2. Checks every edit against the current generation (`check`); a bad edit rejects the whole action.
//!   3. Applies the edits, publishes the action at once, and tells the readers waiting on it.
//!
//! With no action for `IDLE`, it runs the writer's idle check, which may start or finish a rebuild.

use std::collections::HashMap;
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
use crate::state::{Shared, WriteJob};

/// Actions waiting for the writer; a full queue makes callers wait.
pub(crate) const WRITE_QUEUE: usize = 1024;

/// With no action for this long, the writer runs its idle check.
const IDLE: Duration = Duration::from_millis(50);


pub(crate) async fn action(
    State(state): State<Shared>,
    Json(action): Json<ActionRequest>,
) -> ApiResult<ActionResponse> {
    let writer_stopped = || ApiError(StatusCode::SERVICE_UNAVAILABLE, "writer stopped".into());

    let (reply, result) = oneshot::channel();

    state
        .writes
        .send(WriteJob { action, reply })
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

    let mut writer = store.writer();

    runtime.block_on(async {
        loop {
            let job = match tokio::time::timeout(IDLE, queue.recv()).await {
                Ok(Some(job)) => job,

                // Every sender is gone: the server is shutting down.
                Ok(None) => break,

                Err(_) => {
                    writer.idle();
                    continue;
                }
            };

            let seq = job.action.seq;
            let result = apply(&mut writer, job.action);

            if result.is_ok() {
                let _ = published.send(seq);
            }

            let _ = job.reply.send(result);
        }
    });
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
