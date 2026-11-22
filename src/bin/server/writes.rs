//! Writes: actions go through a bounded queue to the one writer thread, which applies each whole and publishes it.

use std::collections::HashMap;
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use tokio::sync::{mpsc, oneshot, watch};

use proximadb::Store;
use proximadb::core::node::GlobalId;
use proximadb::storage::node_type;

use crate::api::{ActionRequest, ActionResponse, ApiError, ApiResult, bad};
use crate::check::{Checked, Target, check_action};
use crate::state::Shared;

/// Writes waiting for the writer; a full queue makes callers wait.
pub(crate) const WRITE_QUEUE: usize = 1024;
/// With no action for this long, the writer runs its idle check.
const IDLE: Duration = Duration::from_millis(50);

pub(crate) async fn action(State(state): State<Shared>, Json(req): Json<ActionRequest>) -> ApiResult<ActionResponse> {
    let (tx, rx) = oneshot::channel();
    state
        .writes
        .send((req, tx))
        .await
        .map_err(|_| ApiError(StatusCode::SERVICE_UNAVAILABLE, "writer stopped".into()))?;
    match rx.await {
        Ok(Ok(done)) => Ok(Json(done)),
        Ok(Err(message)) => Err(bad(message)),
        Err(_) => Err(ApiError(StatusCode::SERVICE_UNAVAILABLE, "writer stopped".into())),
    }
}

pub(crate) fn writer_thread(
    store: &'static Store,
    mut rx: mpsc::Receiver<(ActionRequest, oneshot::Sender<Result<ActionResponse, String>>)>,
    published: watch::Sender<u64>,
) {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
    let mut writer = store.writer();
    runtime.block_on(async {
        loop {
            let (req, reply) = match tokio::time::timeout(IDLE, rx.recv()).await {
                Ok(Some(job)) => job,
                Ok(None) => break,
                Err(_) => {
                    writer.idle();
                    continue;
                }
            };
            let last = writer.generation().live.published();
            if req.seq <= last {
                let _ = reply.send(Err(format!("seq {} is not after the last published action {last}", req.seq)));
                continue;
            }
            let checked = {
                let view = writer.generation().view();
                check_action(&view, &req.edits)
            };
            let checked = match checked {
                Ok(c) => c,
                Err(message) => {
                    let _ = reply.send(Err(message));
                    continue;
                }
            };
            writer.begin(req.seq);
            let mut new_nodes: HashMap<(node_type::Id, String), GlobalId> = HashMap::new();
            let resolve = |t: &Target, new_nodes: &HashMap<(node_type::Id, String), GlobalId>| match t {
                Target::Existing(g) => *g,
                Target::Added(ty, key) => new_nodes[&(*ty, key.clone())],
            };
            let mut missing_links = 0;
            for c in checked {
                match c {
                    Checked::AddNode(t, key, props) => {
                        let g = writer.add_node(t, key.as_bytes());
                        for (name, v) in &props {
                            writer.set_property(g, name, v);
                        }
                        new_nodes.insert((t, key), g);
                    }
                    Checked::SetProperty(n, name, v) => writer.set_property(resolve(&n, &new_nodes), &name, &v),
                    Checked::DeleteNode(n) => writer.delete_node(resolve(&n, &new_nodes)),
                    Checked::AddLink(r, f, t) => {
                        writer.add_link(r, resolve(&f, &new_nodes), resolve(&t, &new_nodes));
                    }
                    Checked::RemoveLink(r, f, t) => {
                        if !writer.remove_link(r, resolve(&f, &new_nodes), resolve(&t, &new_nodes)) {
                            missing_links += 1;
                        }
                    }
                }
            }
            writer.commit();
            let _ = published.send(req.seq);
            let _ = reply.send(Ok(ActionResponse { seq: req.seq, missing_links }));
        }
    });
}
