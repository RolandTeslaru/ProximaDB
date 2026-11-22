//! `POST /cypher`: a Cypher query, answered as a graph to draw (nodes and links) and a count.
//!
//! Each node has an `id` of `"Type:key"`; links refer to nodes by that id, the shape graph-drawing libraries take.
//! Titles and other properties come from Quarry's object store, by type and key.

use std::collections::HashMap;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use proximadb::View;
use proximadb::core::Value;
use proximadb::core::node::GlobalId;
use proximadb::query::cypher;
use proximadb::query::subgraph::{Role, Subgraph};

use crate::api::{ApiError, JsonValue, TimedResult, timed};
use crate::state::{Shared, on_pool, wait_for};


#[derive(Deserialize)]
pub(crate) struct CypherRequest {
    query: String,
    #[serde(default)]
    params: HashMap<String, JsonValue>,
    #[serde(default)]
    after: Option<u64>,
}


#[derive(Serialize)]
pub(crate) struct CypherResponse {
    nodes: Vec<NodeOut>,
    links: Vec<LinkOut>,
    /// Matches in all; the graph may show fewer (`truncated`).
    count: Option<u64>,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    path_length: Option<u32>,
    seq: u64,
}


#[derive(Serialize)]
struct NodeOut {
    id: String,
    #[serde(rename = "type")]
    node_type: String,
    key: String,
    role: &'static str,
}


#[derive(Serialize)]
struct LinkOut {
    #[serde(rename = "type")]
    link_type: String,
    from: String,
    to: String,
}


pub(crate) async fn cypher(
    State(state): State<Shared>,
    Json(request): Json<CypherRequest>,
) -> TimedResult<CypherResponse> {
    wait_for(&state, request.after).await?;

    let store = state.store;

    let (result, took) = on_pool(&state, move || {
        let generation = store.current();
        let view = generation.view();

        answer(&view, &request)
    })
    .await;

    timed(result, took)
}


fn answer(view: &View, request: &CypherRequest) -> Result<Json<CypherResponse>, ApiError> {
    let params: HashMap<String, Value> = request
        .params
        .iter()
        .map(|(name, value)| (name.clone(), value.into()))
        .collect();

    let answer = cypher::run(view, &request.query, &params)
        .map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))?;

    let graph = answer.graph.unwrap_or_default();

    Ok(Json(CypherResponse {
        nodes: nodes_of(view, &graph),
        links: links_of(view, &graph),
        count: answer.count,
        truncated: graph.truncated,
        path_length: answer.path_length,
        seq: view.upto,
    }))
}


fn nodes_of(view: &View, graph: &Subgraph) -> Vec<NodeOut> {
    graph
        .nodes
        .iter()
        .map(|&(node, role)| {
            let (node_type, key) = name_of(view, node);

            NodeOut {
                id: format!("{node_type}:{key}"),
                node_type,
                key,
                role: match role {
                    Role::Start => "start",
                    Role::Match => "match",
                    Role::Via => "via",
                },
            }
        })
        .collect()
}


fn links_of(view: &View, graph: &Subgraph) -> Vec<LinkOut> {
    graph
        .links
        .iter()
        .map(|link| {
            let (from_type, from_key) = name_of(view, link.from);
            let (to_type, to_key) = name_of(view, link.to);

            LinkOut {
                link_type: view.graph.link_types[link.link_type].name.clone(),
                from: format!("{from_type}:{from_key}"),
                to: format!("{to_type}:{to_key}"),
            }
        })
        .collect()
}


/// A node's type name and key.
fn name_of(view: &View, node: GlobalId) -> (String, String) {
    let node_type = view.type_of(node);
    let name = view.graph.node_types[node_type].name.clone();
    let key = String::from_utf8_lossy(&view.key(node)).into_owned();

    (name, key)
}
