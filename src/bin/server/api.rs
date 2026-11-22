//! Request and response shapes, and the error a handler returns.

use std::collections::HashMap;

use axum::Json;
use axum::http::{HeaderName, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use proximadb::core::{Direction, Value};

pub(crate) struct ApiError(pub(crate) StatusCode, pub(crate) String);


impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}


pub(crate) fn bad(message: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, message.into())
}


pub(crate) type ApiResult<T> = Result<Json<T>, ApiError>;


/// A read's answer with the time the engine spent on it, in microseconds, as `x-proximadb-us`: the rest of a
/// round trip is the transport.
pub(crate) type TimedResult<T> = Result<([(HeaderName, String); 1], Json<T>), ApiError>;


pub(crate) fn timed<T>(result: ApiResult<T>, took: std::time::Duration) -> TimedResult<T> {
    result.map(|json| {
        (
            [(
                HeaderName::from_static("x-proximadb-us"),
                took.as_micros().to_string(),
            )],
            json,
        )
    })
}


#[derive(Deserialize, Clone)]
pub(crate) struct NodeRef {
    #[serde(rename = "type")]
    pub(crate) node_type: String,
    pub(crate) key: String,
}


#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Dir {
    Forward,
    Backward,
    Both,
}


impl From<Dir> for Direction {
    fn from(direction: Dir) -> Self {
        match direction {
            Dir::Forward => Direction::Forward,
            Dir::Backward => Direction::Backward,
            Dir::Both => Direction::Both,
        }
    }
}


#[derive(Deserialize, Clone)]
#[serde(untagged)]
pub(crate) enum JsonValue {
    Bool(bool),
    Num(f64),
    Str(String),
}


impl From<&JsonValue> for Value {
    fn from(value: &JsonValue) -> Self {
        match value {
            JsonValue::Bool(flag) => Value::Bool(*flag),
            JsonValue::Num(number) => Value::Num(*number),
            JsonValue::Str(text) => Value::Str(text.clone()),
        }
    }
}


/// `{"property": "status", "op": "eq", "value": "active"}`, `{"has": "name"}`, `{"and": [..]}`, `{"or": [..]}`,
/// `{"not": {..}}`.
#[derive(Deserialize)]
#[serde(untagged)]
pub(crate) enum JsonCondition {
    Compare {
        property: String,
        op: String,
        value: JsonValue,
    },
    Has {
        has: String,
    },
    And {
        and: Vec<JsonCondition>,
    },
    Or {
        or: Vec<JsonCondition>,
    },
    Not {
        not: Box<JsonCondition>,
    },
}


/// A condition on the nodes of one type.
#[derive(Deserialize)]
pub(crate) struct Filter {
    #[serde(rename = "type")]
    pub(crate) node_type: String,
    #[serde(rename = "where")]
    pub(crate) condition: JsonCondition,
}


#[derive(Deserialize)]
pub(crate) struct ReachRequest {
    pub(crate) start: Vec<NodeRef>,
    /// Link type names; empty or missing means all.
    #[serde(default)]
    pub(crate) link_types: Vec<String>,
    pub(crate) direction: Dir,
    pub(crate) max_steps: u32,
    #[serde(default)]
    pub(crate) cap: Option<u64>,
    /// Only step onto nodes of these types that meet their condition; nodes of other types only if
    /// `through_others` is true.
    #[serde(default)]
    pub(crate) through: Vec<Filter>,
    #[serde(default)]
    pub(crate) through_others: bool,
    /// Count (and return) only the reached nodes that meet this.
    #[serde(default, rename = "where")]
    pub(crate) end: Option<Filter>,
    /// Return up to this many keys of the nodes counted.
    #[serde(default)]
    pub(crate) keys: usize,
    #[serde(default)]
    pub(crate) after: Option<u64>,
}


#[derive(Serialize)]
pub(crate) struct ReachResponse {
    pub(crate) count: u64,
    pub(crate) reached: u64,
    pub(crate) capped: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) keys: Vec<KeyOut>,
    pub(crate) seq: u64,
}


#[derive(Serialize)]
pub(crate) struct KeyOut {
    #[serde(rename = "type")]
    pub(crate) node_type: String,
    pub(crate) key: String,
}


#[derive(Deserialize)]
pub(crate) struct PathRequest {
    pub(crate) from: NodeRef,
    pub(crate) to: NodeRef,
    #[serde(default)]
    pub(crate) link_types: Vec<String>,
    pub(crate) direction: Dir,
    pub(crate) max_steps: u32,
    #[serde(default)]
    pub(crate) through: Vec<Filter>,
    #[serde(default)]
    pub(crate) through_others: bool,
    #[serde(default)]
    pub(crate) after: Option<u64>,
}


#[derive(Serialize)]
pub(crate) struct PathResponse {
    pub(crate) length: Option<u32>,
    pub(crate) seq: u64,
}


/// One action: Quarry's sequence number (strictly increasing) and its edits, applied and published together.
#[derive(Deserialize, Clone)]
pub(crate) struct ActionRequest {
    pub(crate) seq: u64,
    pub(crate) edits: Vec<Edit>,
}


#[derive(Deserialize, Clone)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum Edit {
    AddNode {
        #[serde(rename = "type")]
        node_type: String,
        key: String,
        #[serde(default)]
        properties: HashMap<String, JsonValue>,
    },
    SetProperty {
        node: NodeRef,
        property: String,
        value: JsonValue,
    },
    DeleteNode {
        node: NodeRef,
    },
    AddLink {
        link_type: String,
        from: NodeRef,
        to: NodeRef,
    },
    RemoveLink {
        link_type: String,
        from: NodeRef,
        to: NodeRef,
    },
}


#[derive(Serialize)]
pub(crate) struct ActionResponse {
    pub(crate) seq: u64,
    /// Links asked to be removed that did not exist.
    pub(crate) missing_links: u32,
}
