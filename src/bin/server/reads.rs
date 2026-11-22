//! Reads: each runs on the rayon pool against the current generation.

use axum::Json;
use axum::extract::State;

use proximadb::core::node::GlobalId;
use proximadb::query::condition;
use proximadb::query::walk::{self, Reach};
use proximadb::storage::node_type;
use proximadb::Bitset;

use crate::api::{ApiResult, KeyOut, PathRequest, PathResponse, ReachRequest, ReachResponse};
use crate::check::{condition_for, filters_for, find, link_types_of, node_type_of, through_for};
use crate::state::{Shared, on_pool, wait_for};

/// Most keys a reach returns; counts are always exact.
const MAX_KEYS: usize = 100_000;

pub(crate) async fn health(State(state): State<Shared>) -> Json<serde_json::Value> {
    let generation = state.store.current();
    let view = generation.view();
    Json(serde_json::json!({ "ok": true, "nodes": view.node_count(), "seq": view.upto }))
}

pub(crate) async fn reach(State(state): State<Shared>, Json(req): Json<ReachRequest>) -> ApiResult<ReachResponse> {
    wait_for(&state, req.after).await?;
    let store = state.store;
    on_pool(&state, move || {
        let generation = store.current();
        let view = generation.view();
        let graph = view.graph;
        let starts = req.start.iter().map(|r| find(&view, r)).collect::<Result<Vec<_>, _>>()?;
        let link_types = link_types_of(graph, &req.link_types)?;
        let through = through_for(&view, filters_for(graph, &req.through)?, req.through_others);
        let end = match &req.end {
            Some(f) => {
                let t = node_type_of(graph, &f.node_type)?;
                let c = condition_for(graph, t, &f.condition)?;
                Some((t, condition::evaluate(&view, t, &c)))
            }
            None => None,
        };
        let r = walk::reach(
            &view,
            &Reach {
                starts: &starts,
                link_types: &link_types,
                direction: req.direction.into(),
                max_steps: req.max_steps,
                through: through.as_ref(),
                cap: req.cap,
            },
        );
        let matches = |g: GlobalId, end: &Option<(node_type::Id, Bitset)>| match end {
            Some((t, allowed)) => {
                let (gt, local) = view.get(g);
                gt == *t && allowed.get(local.index())
            }
            None => true,
        };
        let mut count = 0u64;
        let mut keys = Vec::new();
        let want = req.keys.min(MAX_KEYS);
        for &g in &r.reached {
            if matches(g, &end) {
                count += 1;
                if keys.len() < want {
                    let t = view.type_of(g);
                    keys.push(KeyOut {
                        node_type: graph.node_types[t].name.clone(),
                        key: String::from_utf8_lossy(&view.key(g)).into_owned(),
                    });
                }
            }
        }
        Ok(Json(ReachResponse { count, reached: r.reached.len() as u64, capped: r.capped, keys, seq: view.upto }))
    })
    .await
}

pub(crate) async fn path(State(state): State<Shared>, Json(req): Json<PathRequest>) -> ApiResult<PathResponse> {
    wait_for(&state, req.after).await?;
    let store = state.store;
    on_pool(&state, move || {
        let generation = store.current();
        let view = generation.view();
        let graph = view.graph;
        let (from, to) = (find(&view, &req.from)?, find(&view, &req.to)?);
        let link_types = link_types_of(graph, &req.link_types)?;
        let through = through_for(&view, filters_for(graph, &req.through)?, req.through_others);
        let length = walk::path_length(&view, from, to, &link_types, req.direction.into(), req.max_steps, through.as_ref());
        Ok(Json(PathResponse { length, seq: view.upto }))
    })
    .await
}
