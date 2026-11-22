//! What every handler shares: the store, the read slots, the writer's queue and the last published action.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use tokio::sync::{Semaphore, mpsc, oneshot, watch};

use proximadb::Store;

use crate::api::{ActionRequest, ActionResponse, ApiError};

/// How long a read with `after: N` waits for action N before giving up.
const AFTER_TIMEOUT: Duration = Duration::from_secs(10);


/// One action for the writer thread, and where its result goes.
pub(crate) struct WriteJob {
    pub(crate) action: ActionRequest,
    pub(crate) reply: oneshot::Sender<Result<ActionResponse, String>>,
}


pub(crate) struct AppState {
    pub(crate) store: &'static Store,
    pub(crate) reads: Semaphore,
    pub(crate) writes: mpsc::Sender<WriteJob>,
    pub(crate) published: watch::Receiver<u64>,
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
