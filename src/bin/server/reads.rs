//! Reads: each runs on the rayon pool against the generation current when it starts.

use axum::Json;
use axum::extract::State;

use proximadb::Bitset;
use proximadb::View;
use proximadb::core::node::GlobalId;
use proximadb::query::condition;
use proximadb::query::walk::{self, Reach};
use proximadb::storage::node_type;

use crate::api::{
    ApiError, Filter, KeyOut, PathRequest, PathResponse, ReachRequest, ReachResponse, TimedResult,
    timed,
};
use crate::check::{condition_for, filters_for, find, link_types_of, node_type_of, through_for};
use crate::state::{Shared, on_pool, wait_for};

/// Most keys a reach returns; counts are always exact.
const MAX_KEYS: usize = 100_000;


pub(crate) async fn health(State(state): State<Shared>) -> Json<serde_json::Value> {
    let generation = state.store.current();
    let view = generation.view();

    Json(serde_json::json!({
        "ok": true,
        "nodes": view.node_count(),
        "seq": view.upto,
    }))
}


/// Who can be reached from the start nodes: walk, then count (and name) the nodes that meet `where`.
pub(crate) async fn reach(
    State(state): State<Shared>,
    Json(request): Json<ReachRequest>,
) -> TimedResult<ReachResponse> {
    wait_for(&state, request.after).await?;

    let store = state.store;

    let (result, took) = on_pool(&state, move || {
        let generation = store.current();
        let view = generation.view();

        reach_in(&view, &request)
    })
    .await;

    timed(result, took)
}


fn reach_in(view: &View, request: &ReachRequest) -> Result<Json<ReachResponse>, ApiError> {
    let graph = view.graph;

    let starts = request
        .start
        .iter()
        .map(|node| find(view, node))
        .collect::<Result<Vec<_>, _>>()?;
    let link_types = link_types_of(graph, &request.link_types)?;
    let through = through_for(
        view,
        filters_for(graph, &request.through)?,
        request.through_others,
    );
    let end = end_filter(view, request.end.as_ref())?;

    let walked = walk::reach(
        view,
        &Reach {
            starts: &starts,
            link_types: &link_types,
            direction: request.direction.into(),
            max_steps: request.max_steps,
            through: through.as_ref(),
            cap: request.cap,
        },
    );

    let wanted_keys = request.keys.min(MAX_KEYS);
    let mut count = 0u64;
    let mut keys = Vec::new();

    for &node in &walked.reached {
        if !meets(view, node, end.as_ref()) {
            continue;
        }

        count += 1;

        if keys.len() < wanted_keys {
            keys.push(key_of(view, node));
        }
    }

    Ok(Json(ReachResponse {
        count,
        reached: walked.reached.len() as u64,
        capped: walked.capped,
        keys,
        seq: view.upto,
    }))
}


/// The `where` of a reach, evaluated over its node type: one yes/no per node of that type.
fn end_filter(
    view: &View,
    filter: Option<&Filter>,
) -> Result<Option<(node_type::Id, Bitset)>, ApiError> {
    let Some(filter) = filter else {
        return Ok(None);
    };

    let node_type = node_type_of(view.graph, &filter.node_type)?;
    let condition = condition_for(view.graph, node_type, &filter.condition)?;

    Ok(Some((
        node_type,
        condition::evaluate(view, node_type, &condition),
    )))
}


fn meets(view: &View, node: GlobalId, end: Option<&(node_type::Id, Bitset)>) -> bool {
    let Some((wanted_type, allowed)) = end else {
        return true;
    };

    let (node_type, position) = view.get(node);

    node_type == *wanted_type && allowed.get(position.index())
}


fn key_of(view: &View, node: GlobalId) -> KeyOut {
    let node_type = view.type_of(node);

    KeyOut {
        node_type: view.graph.node_types[node_type].name.clone(),
        key: String::from_utf8_lossy(&view.key(node)).into_owned(),
    }
}


/// The length of the shortest path between two nodes, optionally only through nodes that meet a filter.
pub(crate) async fn path(
    State(state): State<Shared>,
    Json(request): Json<PathRequest>,
) -> TimedResult<PathResponse> {
    wait_for(&state, request.after).await?;

    let store = state.store;

    let (result, took) = on_pool(&state, move || {
        let generation = store.current();
        let view = generation.view();

        path_in(&view, &request)
    })
    .await;

    timed(result, took)
}


fn path_in(view: &View, request: &PathRequest) -> Result<Json<PathResponse>, ApiError> {
    let graph = view.graph;

    let from = find(view, &request.from)?;
    let to = find(view, &request.to)?;
    let link_types = link_types_of(graph, &request.link_types)?;
    let through = through_for(
        view,
        filters_for(graph, &request.through)?,
        request.through_others,
    );

    let length = walk::path_length(
        view,
        from,
        to,
        &link_types,
        request.direction.into(),
        request.max_steps,
        through.as_ref(),
    );

    Ok(Json(PathResponse {
        length,
        seq: view.upto,
    }))
}
