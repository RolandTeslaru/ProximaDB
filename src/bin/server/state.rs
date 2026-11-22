//! What every handler shares: the store, the read slots, the writer's queue and the last published action.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use tokio::sync::{Semaphore, mpsc, oneshot, watch};

use proximadb::Store;
use proximadb::storage::Graph;

use crate::api::{ActionRequest, ActionResponse, ApiError};

/// How long a read with `after: N` waits for action N before giving up.
const AFTER_TIMEOUT: Duration = Duration::from_secs(10);


/// Work for the writer thread, and where its result goes.
pub(crate) enum WriteJob {
    Action {
        action: ActionRequest,
        reply: oneshot::Sender<Result<ActionResponse, String>>,
    },

    /// A freshly loaded graph holding every action up to `as_of_seq`, to take the current one's place.
    Swap {
        graph: Box<Graph>,
        as_of_seq: u64,
        reply: oneshot::Sender<Result<Swapped, String>>,
    },
}


/// What swapping in a reloaded graph did.
pub(crate) struct Swapped {
    /// Kept actions after `as_of_seq`, applied again on the new graph.
    pub(crate) replayed: usize,
    /// Of those, the ones that no longer apply (say, a link to a node the new data doesn't have).
    pub(crate) failed: Vec<String>,
    /// The last action the new graph holds.
    pub(crate) seq: u64,
}


/// Where the data comes from, for reloads: the schema file is read again each time.
pub(crate) struct Source {
    pub(crate) schema: PathBuf,
    pub(crate) data: PathBuf,
}


pub(crate) struct AppState {
    pub(crate) store: &'static Store,
    pub(crate) reads: Semaphore,
    pub(crate) writes: mpsc::Sender<WriteJob>,
    pub(crate) published: watch::Receiver<u64>,
    pub(crate) source: Source,
    /// A reload is running; another one is refused until it ends.
    pub(crate) reloading: AtomicBool,
}


pub(crate) type Shared = Arc<AppState>;


/// Runs `read` on the rayon pool, holding one read slot; returns its result and how long it ran.
pub(crate) async fn on_pool<T: Send + 'static>(
    state: &Shared,
    read: impl FnOnce() -> T + Send + 'static,
) -> (T, Duration) {
    let _slot = state
        .reads
        .acquire()
        .await
        .expect("the semaphore is never closed");

    let (sender, receiver) = oneshot::channel();

    rayon::spawn(move || {
        let started = Instant::now();
        let result = read();

        let _ = sender.send((result, started.elapsed()));
    });

    receiver.await.expect("a read always sends its result")
}


/// Waits until action `after` is published, for read-your-writes.
pub(crate) async fn wait_for(state: &Shared, after: Option<u64>) -> Result<(), ApiError> {
    let Some(seq) = after else {
        return Ok(());
    };

    let mut published = state.published.clone();
    let wait = published.wait_for(|&last| last >= seq);

    tokio::time::timeout(AFTER_TIMEOUT, wait)
        .await
        .map_err(|_| {
            ApiError(
                StatusCode::GATEWAY_TIMEOUT,
                format!(
                    "action {seq} not published within {} s",
                    AFTER_TIMEOUT.as_secs()
                ),
            )
        })?
        .map_err(|_| ApiError(StatusCode::SERVICE_UNAVAILABLE, "writer stopped".into()))?;

    Ok(())
}
