//! What every handler shares: the store, the read slots, the writer's queue and the last published action.

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use tokio::sync::{Semaphore, mpsc, oneshot, watch};

use proximadb::Store;

use crate::api::{ActionRequest, ActionResponse, ApiError};

pub(crate) struct AppState {
    pub(crate) store: &'static Store,
    pub(crate) reads: Semaphore,
    pub(crate) writes: mpsc::Sender<(ActionRequest, oneshot::Sender<Result<ActionResponse, String>>)>,
    pub(crate) published: watch::Receiver<u64>,
}

pub(crate) type Shared = Arc<AppState>;

/// Runs `f` on the rayon pool, holding one read slot, and waits for it without blocking tokio.
pub(crate) async fn on_pool<T: Send + 'static>(state: &Shared, f: impl FnOnce() -> T + Send + 'static) -> T {
    let _slot = state.reads.acquire().await.expect("semaphore open");
    let (tx, rx) = oneshot::channel();
    rayon::spawn(move || {
        let _ = tx.send(f());
    });
    rx.await.expect("read finished")
}

pub(crate) async fn wait_for(state: &Shared, after: Option<u64>) -> Result<(), ApiError> {
    if let Some(n) = after {
        let mut rx = state.published.clone();
        let wait = rx.wait_for(|&p| p >= n);
        tokio::time::timeout(Duration::from_secs(10), wait)
            .await
            .map_err(|_| ApiError(StatusCode::GATEWAY_TIMEOUT, format!("action {n} not published within 10 s")))?
            .map_err(|_| ApiError(StatusCode::SERVICE_UNAVAILABLE, "writer stopped".into()))?;
    }
    Ok(())
}
