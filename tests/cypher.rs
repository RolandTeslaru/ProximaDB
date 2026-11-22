//! Cypher on a graph small enough to check every answer by hand.
//!
//! | Person | status   | age |
//! |--------|----------|-----|
//! | ana    | active   | 30  |
//! | ben    | active   | 41  |
//! | cleo   | inactive | 25  |
//! | dan    | active   | 52  |
//! | eve    | active   | 19  |
//!
//! Follows: ana → ben, ana → cleo, ben → dan, cleo → eve, dan → eve. Owns: ana → phone, dan → laptop.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::PathBuf;

use proximadb::Live;
use proximadb::core::Kind;
use proximadb::load::{self, LinkTypeSpec, NodeTypeSpec, Schema};
use proximadb::query::cypher::{self, Answer};
use proximadb::query::subgraph::Role;
use proximadb::storage::Graph;


fn graph() -> Graph {
    let dir = std::env::temp_dir().join(format!("proximadb-cypher-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();

    fs::write(
        dir.join("Person.csv"),
        "key,status,age\nana,active,30\nben,active,41\ncleo,inactive,25\ndan,active,52\neve,active,19\n",
    )
    .unwrap();
    fs::write(dir.join("Thing.csv"), "key\nphone\nlaptop\n").unwrap();
    fs::write(
        dir.join("FOLLOWS.csv"),
        "from,to\nana,ben\nana,cleo\nben,dan\ncleo,eve\ndan,eve\n",
    )
    .unwrap();
    fs::write(dir.join("OWNS.csv"), "from,to\nana,phone\ndan,laptop\n").unwrap();

    let schema = Schema {
        node_types: vec![
            NodeTypeSpec {
                name: "Person".into(),
                file: "Person.csv".into(),
                primary_key: "key".into(),
                properties: vec![
                    ("status".into(), Kind::String),
                    ("age".into(), Kind::Integer),
                ],
            },
            NodeTypeSpec {
                name: "Thing".into(),
                file: "Thing.csv".into(),
                primary_key: "key".into(),
                properties: vec![],
            },
        ],
        link_types: vec![
            link_type("FOLLOWS", "Person", "Person"),
            link_type("OWNS", "Person", "Thing"),
        ],
    };

    let graph = load::load(&PathBuf::from(&dir), &schema).unwrap();
    fs::remove_dir_all(&dir).unwrap();

    graph
}


fn link_type(name: &str, from: &str, to: &str) -> LinkTypeSpec {
    LinkTypeSpec {
        name: name.into(),
        from_type: from.into(),
        to_type: to.into(),
        file: format!("{name}.csv"),
        from_column: "from".into(),
        to_column: "to".into(),
    }
}


fn run(graph: &Graph, query: &str) -> Answer {
    let live = Live::new(graph);
    let view = live.view(graph);

    cypher::run(&view, query, &HashMap::new()).unwrap_or_else(|e| panic!("{query}: {e}"))
}


fn error(graph: &Graph, query: &str) -> String {
    let live = Live::new(graph);
    let view = live.view(graph);

    match cypher::run(&view, query, &HashMap::new()) {
        Ok(_) => panic!("{query}: expected an error"),
        Err(e) => e.message,
    }
}


/// The keys of the nodes with `role` in the answer's graph.
fn keys(graph: &Graph, answer: &Answer, role: Role) -> BTreeSet<String> {
    answer
        .graph
        .as_ref()
        .unwrap()
        .nodes
        .iter()
        .filter(|&&(_, r)| r == role)
        .map(|&(node, _)| String::from_utf8(graph.keys.get(node).to_vec()).unwrap())
        .collect()
}


#[test]
fn walks_count_and_draw() {
    let g = graph();

    // Everyone ana reaches by following: ben, cleo (1 step), dan, eve (2 steps).
    let all = run(
        &g,
        "MATCH (a:Person {key: 'ana'})-[:FOLLOWS*1..3]->(b:Person) RETURN b",
    );
    assert_eq!(all.count, Some(4));
    assert_eq!(
        keys(&g, &all, Role::Match),
        ["ben", "cleo", "dan", "eve"].map(String::from).into()
    );
    assert_eq!(
        all.graph.as_ref().unwrap().links.len(),
        4,
        "a tree: one link per node reached"
    );

    // Exactly two steps away.
    let two = run(
        &g,
        "MATCH (a:Person {key: 'ana'})-[:FOLLOWS*2]->(b:Person) RETURN count(b)",
    );
    assert_eq!(two.count, Some(2));
    assert!(two.graph.is_none(), "a count alone draws nothing");

    // End filter: over 40 is ben and dan.
    let older = run(
        &g,
        "MATCH (a:Person {key: 'ana'})-[:FOLLOWS*1..3]->(b:Person) WHERE b.age > 40 RETURN b",
    );
    assert_eq!(older.count, Some(2));
    assert_eq!(
        keys(&g, &older, Role::Match),
        ["ben", "dan"].map(String::from).into()
    );

    // Path filter: only through active people, so not through cleo; eve is still reached through dan.
    let active = "MATCH (a:Person {key: 'ana'})-[:FOLLOWS*1..3 (r, n | n.status = 'active')]->(b:Person) RETURN b";
    let through_active = run(&g, active);
    assert_eq!(
        keys(&g, &through_active, Role::Match),
        ["ben", "dan", "eve"].map(String::from).into()
    );

    // A match reached through a node that isn't one shows that node as on the way.
    let eve = run(
        &g,
        "MATCH (a:Person {key: 'ana'})-[:FOLLOWS*1..3]->(b:Person) WHERE b.age < 20 RETURN b",
    );
    assert_eq!(
        keys(&g, &eve, Role::Match),
        ["eve"].map(String::from).into()
    );
    assert_eq!(keys(&g, &eve, Role::Via), ["cleo"].map(String::from).into());

    // Any link type, both ways, one step: ben, cleo and the phone.
    let around = run(&g, "MATCH (a:Person {key: 'ana'})-[r]-(b) RETURN a, r, b");
    assert_eq!(around.count, Some(3));

    // LIMIT draws fewer but counts all.
    let limited = run(
        &g,
        "MATCH (a:Person {key: 'ana'})-[:FOLLOWS*1..3]->(b:Person) RETURN b LIMIT 1",
    );
    assert_eq!(limited.count, Some(4));
    assert_eq!(keys(&g, &limited, Role::Match).len(), 1);
    assert!(limited.graph.as_ref().unwrap().truncated);
}


#[test]
fn shortest_paths() {
    let g = graph();

    let to_eve = "MATCH p = shortestPath((a:Person {key: 'ana'})-[:FOLLOWS*..6]->(b:Person {key: 'eve'})) RETURN p";
    let path = run(&g, to_eve);
    assert_eq!(path.path_length, Some(2));
    assert_eq!(
        keys(&g, &path, Role::Via),
        ["cleo"].map(String::from).into()
    );

    // Through active people only, the way to eve goes through ben and dan.
    let active = "MATCH p = shortestPath((a:Person {key: 'ana'})-[:FOLLOWS*..6 (r, n | n.status = 'active')]->\
                  (b:Person {key: 'eve'})) RETURN p";
    let around = run(&g, active);
    assert_eq!(around.path_length, Some(3));
    assert_eq!(
        keys(&g, &around, Role::Via),
        ["ben", "dan"].map(String::from).into()
    );

    // Nobody follows ana.
    let none = "MATCH p = shortestPath((a:Person {key: 'eve'})-[:FOLLOWS*..6]->(b:Person {key: 'ana'})) RETURN p";
    assert_eq!(run(&g, none).path_length, None);
}


#[test]
fn errors_say_what_is_wrong() {
    let g = graph();

    let cases = [
        (
            "MATCH (a:Person {key: 'ana'})-[:FOLLOWS*]->(b) RETURN b",
            "maximum length",
        ),
        (
            "MATCH (a:Person {key: 'nobody'}) RETURN a",
            "no Person with key nobody",
        ),
        (
            "MATCH (a:Person {key: 'ana'})-[:FOLLOWS]->(b) WHERE b.age > 1 RETURN b",
            "label the end node",
        ),
        (
            "MATCH (a:Person {key: 'ana'})-[:FOLLOWS]->(b:Person) WHERE b.status > 'a' RETURN b",
            "= and <>",
        ),
        (
            "MATCH (a:Person {key: 'ana'})-[:FOLLOWS]->(b:Person) WHERE b.age = 'old' RETURN b",
            "wrong type",
        ),
        (
            "MATCH (a:Person)-[:FOLLOWS]->(b:Person) WHERE a.age > 40 OR b.age > 40 RETURN b",
            "about one node",
        ),
        (
            "MATCH (a:Person)-[:FOLLOWS]->(b:Person) RETURN count(a)",
            "count the end node",
        ),
        ("MATCH (a)-[:FOLLOWS]->(b) RETURN b", "start node's label"),
        (
            "MATCH (a:Person {key: 'ana'})-[:LIKES]->(b) RETURN b",
            "no link type LIKES",
        ),
        (
            "MATCH (a:Person {key: 'ana'})-[:FOLLOWS]->(b) RETURN c",
            "isn't named",
        ),
        (
            "MATCH (a:Person {key: 'ana'})-[:FOLLOWS]->(b) RETURN b.age",
            "return the node itself",
        ),
        (
            "MATCH (a:Person {key: 'ana'}) RETURN a extra",
            "end of the query",
        ),
        ("MATCH (a:Person {key: 'ana'", "expected"),
        ("MATCH", "expected ( starting a node"),
    ];

    for (query, expected) in cases {
        let message = error(&g, query);

        assert!(
            message.contains(expected),
            "{query}\n  said: {message}\n  expected: {expected}"
        );
    }
}


#[test]
fn starting_from_every_node_that_matches() {
    let g = graph();

    // A filter alone: people over 40 are ben and dan.
    let older = run(&g, "MATCH (a:Person) WHERE a.age > 40 RETURN a");
    assert_eq!(older.count, Some(2));
    assert_eq!(
        keys(&g, &older, Role::Match),
        ["ben", "dan"].map(String::from).into()
    );

    // Inline properties are equalities: everyone active but cleo.
    let active = run(&g, "MATCH (a:Person {status: 'active'}) RETURN count(a)");
    assert_eq!(active.count, Some(4));

    // Everyone followed by anyone. Every one of them is also a start: they still count, each once.
    let followed = run(
        &g,
        "MATCH (a:Person)-[:FOLLOWS]->(b:Person) RETURN count(b)",
    );
    assert_eq!(followed.count, Some(4));

    // Whom people over 40 follow: ben → dan, dan → eve. Ben appears as the start dan was reached from.
    let theirs = run(
        &g,
        "MATCH (a:Person)-[:FOLLOWS]->(b:Person) WHERE a.age > 40 RETURN a, b",
    );
    assert_eq!(theirs.count, Some(2));
    assert_eq!(
        keys(&g, &theirs, Role::Match),
        ["dan", "eve"].map(String::from).into()
    );
    assert_eq!(
        keys(&g, &theirs, Role::Start),
        ["ben"].map(String::from).into()
    );
    assert_eq!(theirs.graph.as_ref().unwrap().links.len(), 2);

    // Filters on both ends: active people followed by someone over 40.
    let both = run(
        &g,
        "MATCH (a:Person)-[:FOLLOWS]->(b:Person) WHERE a.age > 40 AND b.status = 'active' RETURN count(b)",
    );
    assert_eq!(both.count, Some(2));

    // Backwards from things: who owns anything.
    let owners = run(&g, "MATCH (t:Thing)<-[:OWNS]-(p:Person) RETURN p");
    assert_eq!(
        keys(&g, &owners, Role::Match),
        ["ana", "dan"].map(String::from).into()
    );

    // Distance 0 includes the starts themselves: cleo, and eve whom she follows.
    let with_self = run(
        &g,
        "MATCH (a:Person {status: 'inactive'})-[:FOLLOWS*0..1]->(b:Person) RETURN count(b)",
    );
    assert_eq!(with_self.count, Some(2));
}
