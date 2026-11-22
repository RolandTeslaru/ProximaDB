//! `POST /reload {"as_of_seq": N}`: load the datasets again, for when Quarry's sync (DuckDB) has rewritten them.
//!
//!   1. Read the schema file again (it may have changed) and load every dataset, on a blocking thread: searches and
//!      actions carry on against the current graph meanwhile.
//!   2. Hand the new graph to the writer thread, which applies again the kept actions after `N` (the datasets hold
//!      everything up to `N`), then moves searches to it.
//!   3. Answer with what was loaded, what was skipped, and which replayed actions no longer apply.
//!
//! One reload runs at a time; another gets a 409 until it ends.

use std::sync::atomic::Ordering;
use std::time::Instant;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use proximadb::load;

use crate::api::{ApiError, ApiResult};
use crate::schema;
use crate::state::{Shared, WriteJob};


#[derive(Deserialize)]
pub(crate) struct ReloadRequest {
    /// The last of Quarry's actions the datasets already hold.
    as_of_seq: u64,
}


#[derive(Serialize)]
pub(crate) struct ReloadResponse {
    nodes: usize,
    links: usize,
    load_seconds: f64,
    /// Rows and links the load skipped, as messages (no key, duplicate key, empty or dangling link keys).
    skipped: Vec<String>,
    /// Kept actions after `as_of_seq` applied again on the new graph, and the ones that no longer apply.
    replayed: usize,
    failed: Vec<String>,
    /// The last action the new graph holds.
    seq: u64,
}


pub(crate) async fn reload(
    State(state): State<Shared>,
    Json(request): Json<ReloadRequest>,
) -> ApiResult<ReloadResponse> {
    let already_running = state.reloading.swap(true, Ordering::AcqRel);

    if already_running {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "a reload is already running".into(),
        ));
    }

    let result = reload_and_swap(&state, request.as_of_seq).await;

    state.reloading.store(false, Ordering::Release);

    result
}


async fn reload_and_swap(state: &Shared, as_of_seq: u64) -> ApiResult<ReloadResponse> {
    let schema_path = state.source.schema.clone();
    let data = state.source.data.clone();
    let started = Instant::now();

    let loaded = tokio::task::spawn_blocking(move || {
        let schema = schema::schema_from(&schema_path)?;

        load::load_with_report(&data, &schema).map_err(|e| format!("load: {e}"))
    })
    .await
    .map_err(|e| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("the load stopped: {e}"),
        )
    })?;

    let (graph, report) =
        loaded.map_err(|message| ApiError(StatusCode::UNPROCESSABLE_ENTITY, message))?;

    let load_seconds = started.elapsed().as_secs_f64();
    let nodes = graph.node_count();
    let links = graph
        .link_types
        .iter()
        .map(|link_type| link_type.forward.len())
        .sum();

    let writer_stopped = || ApiError(StatusCode::SERVICE_UNAVAILABLE, "writer stopped".into());
    let (reply, result) = oneshot::channel();

    state
        .writes
        .send(WriteJob::Swap {
            graph: Box::new(graph),
            as_of_seq,
            reply,
        })
        .await
        .map_err(|_| writer_stopped())?;

    let swapped = result
        .await
        .map_err(|_| writer_stopped())?
        .map_err(|message| ApiError(StatusCode::CONFLICT, message))?;

    Ok(Json(ReloadResponse {
        nodes,
        links,
        load_seconds,
        skipped: skipped_of(&report),
        replayed: swapped.replayed,
        failed: swapped.failed,
        seq: swapped.seq,
    }))
}


pub(crate) fn skipped_of(report: &load::Report) -> Vec<String> {
    let mut skipped = Vec::new();

    for node_type in &report.node_types {
        if node_type.without_key + node_type.duplicate_keys > 0 {
            skipped.push(format!(
                "{}: {} rows without a key, {} with a duplicate key",
                node_type.name, node_type.without_key, node_type.duplicate_keys
            ));
        }
    }

    for link_type in &report.link_types {
        if link_type.empty + link_type.dangling > 0 {
            skipped.push(format!(
                "{}: {} links, {} with an empty key, {} pointing to no node",
                link_type.name, link_type.links, link_type.empty, link_type.dangling
            ));
        }
    }

    skipped
}
