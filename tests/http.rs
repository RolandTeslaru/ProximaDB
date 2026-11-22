//! The real server binary, over its Unix socket: every route, its errors, read-your-writes, keep-alive, and bad input
//! that must never stop it.

#![cfg(feature = "server")]

mod common;

use std::io::Write;
use std::time::Duration;

use common::server::{Server, person};
use serde_json::json;


#[test]
fn health_and_engine_time() {
    let server = Server::small();
    let mut client = server.client();

    let health = client.get("/health").ok();
    assert_eq!(health["nodes"], 6);
    assert_eq!(health["seq"], 0);

    let response = client.post(
        "/cypher",
        json!({"query": "MATCH (a:Person {key: 'ana'}) RETURN a"}),
    );
    assert!(response.header("x-proximadb-us").is_some());
}


#[test]
fn cypher_graph_count_and_params() {
    let server = Server::small();
    let mut client = server.client();

    let answer = client
        .post(
            "/cypher",
            json!({"query": "MATCH (a:Person {key: $who})-[:FOLLOWS*1..3]->(b:Person) WHERE b.age > 40 RETURN b",
                   "params": {"who": "ana"}}),
        )
        .ok();

    assert_eq!(answer["count"], 2);

    let matches: Vec<&str> = answer["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|node| node["role"] == "match")
        .map(|node| node["key"].as_str().unwrap())
        .collect();
    assert_eq!(matches.len(), 2);
    assert!(matches.contains(&"ben") && matches.contains(&"dan"));

    for link in answer["links"].as_array().unwrap() {
        let ids: Vec<&str> = answer["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["id"].as_str().unwrap())
            .collect();
        assert!(
            ids.contains(&link["from"].as_str().unwrap())
                && ids.contains(&link["to"].as_str().unwrap())
        );
    }

    let path = client
        .post(
            "/cypher",
            json!({"query": "MATCH p = shortestPath((a:Person {key: 'ana'})-[:FOLLOWS*..6]->(b:Person {key: 'eve'})) RETURN p"}),
        )
        .ok();
    assert_eq!(path["path_length"], 2);
}


#[test]
fn reach_and_path() {
    let server = Server::small();
    let mut client = server.client();

    let reach = client
        .post(
            "/reach",
            json!({"start": [person("ana")], "link_types": ["FOLLOWS"], "direction": "forward", "max_steps": 3,
                   "through": [{"type": "Person", "where": {"property": "status", "op": "eq", "value": "active"}}],
                   "keys": 10}),
        )
        .ok();
    assert_eq!(reach["count"], 3);

    let path = client
        .post("/path", json!({"from": person("ana"), "to": person("eve"), "direction": "forward", "max_steps": 6}))
        .ok();
    assert_eq!(path["length"], 2);

    let none = client
        .post("/path", json!({"from": person("eve"), "to": person("ana"), "direction": "forward", "max_steps": 6}))
        .ok();
    assert!(none["length"].is_null());
}


#[test]
fn actions_read_your_writes_and_rejections() {
    let server = Server::small();
    let mut client = server.client();

    let added = client.post(
        "/actions",
        json!({"seq": 1, "edits": [
            {"op": "add_node", "type": "Person", "key": "fay", "properties": {"status": "active", "joined": "2026-01-01"}},
            {"op": "add_link", "link_type": "FOLLOWS", "from": person("eve"), "to": person("fay")}
        ]}),
    );
    assert_eq!(added.ok()["seq"], 1);

    let after = client
        .post("/path", json!({"from": person("ana"), "to": person("fay"), "direction": "forward", "max_steps": 6, "after": 1}))
        .ok();
    assert_eq!(after["length"], 3);

    let cases = [
        (json!({"seq": 1, "edits": []}), "not after"),
        (
            json!({"seq": 2, "edits": [{"op": "add_node", "type": "Person", "key": "ana"}]}),
            "already exists",
        ),
        (
            json!({"seq": 2, "edits": [{"op": "add_node", "type": "Ghost", "key": "x"}]}),
            "no node type",
        ),
        (
            json!({"seq": 2, "edits": [{"op": "set_property", "node": person("ana"), "property": "age", "value": "old"}]}),
            "wrong type",
        ),
        (
            json!({"seq": 2, "edits": [{"op": "set_property", "node": person("ana"), "property": "joined", "value": "yesterday"}]}),
            "not a date",
        ),
        (
            json!({"seq": 2, "edits": [{"op": "add_link", "link_type": "OWNS", "from": person("ana"), "to": person("ben")}]}),
            "joins",
        ),
        (
            json!({"seq": 2, "edits": [{"op": "delete_node", "node": person("nobody")}]}),
            "no Person",
        ),
    ];

    for (body, wanted) in cases {
        let response = client.post("/actions", body.clone());

        assert_eq!(response.status, 400, "{body}: {}", response.body);
        assert!(response.body.contains(wanted), "{body}: {}", response.body);
    }

    // A rejected action changes nothing, even its valid first edit.
    let half = json!({"seq": 2, "edits": [
        {"op": "add_node", "type": "Person", "key": "gus"},
        {"op": "add_node", "type": "Ghost", "key": "x"}
    ]});
    assert_eq!(client.post("/actions", half).status, 400);

    let gus = client.post(
        "/cypher",
        json!({"query": "MATCH (a:Person {key: 'gus'}) RETURN a"}),
    );
    assert_eq!(gus.status, 400);
    assert_eq!(client.get("/health").ok()["seq"], 1);
}


#[test]
fn reload_replays_and_refuses_going_back() {
    let server = Server::small();
    let mut client = server.client();

    client
        .post(
            "/actions",
            json!({"seq": 1, "edits": [{"op": "add_node", "type": "Person", "key": "fay"}]}),
        )
        .ok();
    client
        .post("/actions", json!({"seq": 2, "edits": [{"op": "add_link", "link_type": "FOLLOWS", "from": person("ana"), "to": person("fay")}]}))
        .ok();

    let reloaded = client.post("/reload", json!({"as_of_seq": 0})).ok();
    assert_eq!(reloaded["replayed"], 2);
    assert_eq!(reloaded["failed"], json!([]));

    let count = client
        .post(
            "/cypher",
            json!({"query": "MATCH (a:Person {key: 'ana'})-[:FOLLOWS]->(b) RETURN count(b)"}),
        )
        .ok();
    assert_eq!(count["count"], 3);

    assert_eq!(
        client.post("/reload", json!({"as_of_seq": 2})).ok()["seq"],
        2
    );

    let back = client.post("/reload", json!({"as_of_seq": 0}));
    assert_eq!(back.status, 409, "{}", back.body);
}


#[test]
fn bad_requests_never_stop_the_server() {
    let mut server = Server::small();
    let mut client = server.client();

    let routes = ["/cypher", "/reach", "/path", "/actions", "/reload"];
    let bodies: [&[u8]; 9] = [
        b"",
        b"{",
        b"null",
        b"[]",
        b"{\"query\": 7}",
        b"\xff\xfe",
        b"{\"query\": \"MATCH (a:Person {key: 'ana'})-[*..1100]-(b) RETURN count(b)\"}",
        b"{\"start\": [], \"direction\": \"sideways\", \"max_steps\": -1}",
        b"{\"seq\": 18446744073709551616, \"edits\": []}",
    ];

    for route in routes {
        for body in bodies {
            client.send("POST", route, Some("application/json"), body);
            let response = client.receive().unwrap();

            assert!(
                response.status < 500 || response.status == 503,
                "{route} {:?}: {}",
                body,
                response.status
            );
        }

        client.send("POST", route, Some("text/plain"), b"{}");
        assert_eq!(client.receive().unwrap().status, 415, "{route}");
    }

    assert_eq!(client.get("/nowhere").status, 404);
    assert!(server.is_alive(), "{}", server.log());
}


#[test]
fn garbage_on_the_socket_doesnt_stop_it() {
    let mut server = Server::small();

    for garbage in [
        &b"\0\0\0\0"[..],
        b"GET\r\n\r\n",
        b"POST /cypher HTTP/1.1\r\ncontent-length: 99999999\r\n\r\n{",
    ] {
        let mut raw = std::os::unix::net::UnixStream::connect(&server.socket).unwrap();
        raw.write_all(garbage).unwrap();
        drop(raw);
    }

    std::thread::sleep(Duration::from_millis(200));

    assert!(server.is_alive(), "{}", server.log());
    assert_eq!(server.client().get("/health").status, 200);
}


#[test]
fn many_clients_at_once() {
    let server = Server::small();

    std::thread::scope(|scope| {
        for t in 0..16 {
            let server = &server;

            scope.spawn(move || {
                let mut client = server.client();

                for _ in 0..100 {
                    let answer = client
                        .post("/cypher", json!({"query": "MATCH (a:Person {key: 'ana'})-[:FOLLOWS*1..3]->(b) RETURN count(b)"}))
                        .ok();

                    assert_eq!(answer["count"], 4, "client {t}");
                }
            });
        }
    });
}


#[test]
fn startup_errors_are_messages_not_crashes() {
    let dir = common::Dir::new();
    dir.write("schema.json", "{ not json");

    let output = std::process::Command::new(common::server::binary())
        .args(["--schema"])
        .arg(dir.path().join("schema.json"))
        .args(["--data"])
        .arg(dir.path())
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).starts_with("proximadb: schema"));

    let output = std::process::Command::new(common::server::binary())
        .args(["--nonsense", "x"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown argument"));
}
