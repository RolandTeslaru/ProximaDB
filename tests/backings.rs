//! Link types backed the three ways Quarry backs them, loaded from CSV and checked through Cypher.
//!
//! | Person | team | | Team | | Thing | | Loan (person, thing, since) |
//! |---|---|---|---|---|---|---|---|
//! | ana | red | | red | | phone | | l1: ana, phone, 2024-03-01 |
//! | ben | blue | | blue | | laptop | | l2: ben, laptop, 2025-06-01 |
//! | cleo | green (no such team) | | | | | | l3: cleo, camera (no such thing), 2025-07-01 |
//! | dan | (none) | | | | | | |

mod common;

use std::collections::{BTreeSet, HashMap};
use std::fs;

use proximadb::Live;
use proximadb::core::Kind;
use proximadb::load::{self, Backing, LinkTypeSpec, NodeTypeSpec, Report, Schema, Side};
use proximadb::query::cypher;
use proximadb::query::subgraph::Role;
use proximadb::storage::Graph;


fn graph() -> (Graph, Report) {
    // A folder of this test's own: tests run in parallel.
    let folder = common::Dir::new();
    let dir = folder.path().to_path_buf();

    fs::write(
        dir.join("Person.csv"),
        "key,team\nana,red\nben,blue\ncleo,green\ndan,\n",
    )
    .unwrap();
    fs::write(dir.join("Team.csv"), "key\nred\nblue\n").unwrap();
    fs::write(dir.join("Thing.csv"), "key\nphone\nlaptop\n").unwrap();
    fs::write(
        dir.join("Loan.csv"),
        "key,person,thing,since\nl1,ana,phone,2024-03-01\nl2,ben,laptop,2025-06-01\nl3,cleo,camera,2025-07-01\n",
    )
    .unwrap();

    let node_type = |name: &str, properties: Vec<(String, Kind)>| NodeTypeSpec {
        name: name.into(),
        file: format!("{name}.csv"),
        primary_key: "key".into(),
        properties,
    };

    let schema = Schema {
        node_types: vec![
            node_type("Person", vec![]),
            node_type("Team", vec![]),
            node_type("Thing", vec![]),
            node_type("Loan", vec![("since".into(), Kind::Date)]),
        ],
        link_types: vec![
            LinkTypeSpec {
                name: "MEMBER_OF".into(),
                from_type: "Person".into(),
                to_type: "Team".into(),
                backing: Backing::ForeignKey {
                    side: Side::From,
                    property: "team".into(),
                },
            },
            LinkTypeSpec {
                name: "BORROWED".into(),
                from_type: "Person".into(),
                to_type: "Thing".into(),
                backing: Backing::Object {
                    node_type: "Loan".into(),
                    from_property: "person".into(),
                    to_property: "thing".into(),
                },
            },
        ],
    };

    load::load_with_report(&dir, &schema).unwrap()
}


fn matches(graph: &Graph, query: &str) -> BTreeSet<String> {
    let live = Live::new(graph);
    let view = live.view(graph);

    let answer =
        cypher::run(&view, query, &HashMap::new()).unwrap_or_else(|e| panic!("{query}: {e}"));

    answer
        .graph
        .unwrap()
        .nodes
        .iter()
        .filter(|&&(_, role)| role == Role::Match)
        .map(|&(node, _)| String::from_utf8(graph.keys.get(node).to_vec()).unwrap())
        .collect()
}


fn set(keys: &[&str]) -> BTreeSet<String> {
    keys.iter().map(|key| key.to_string()).collect()
}


#[test]
fn foreign_key_links() {
    let (g, report) = graph();

    assert_eq!(
        matches(
            &g,
            "MATCH (p:Person {key: 'ana'})-[:MEMBER_OF]->(t:Team) RETURN t"
        ),
        set(&["red"])
    );
    assert_eq!(
        matches(
            &g,
            "MATCH (t:Team {key: 'blue'})<-[:MEMBER_OF]-(p:Person) RETURN p"
        ),
        set(&["ben"])
    );

    let member_of = report
        .link_types
        .iter()
        .find(|link_type| link_type.name == "MEMBER_OF")
        .unwrap();

    assert_eq!(member_of.links, 2);
    assert_eq!(member_of.dangling, 1, "cleo's team green doesn't exist");
    assert_eq!(member_of.empty, 1, "dan has no team");
}


#[test]
fn intermediary_object_links() {
    let (g, report) = graph();

    // The intermediary becomes two link types, crossed forward from person to thing.
    let names: Vec<&str> = g
        .link_types
        .iter()
        .map(|link_type| link_type.name.as_str())
        .collect();
    assert!(names.contains(&"BORROWED.a") && names.contains(&"BORROWED.b"));

    let borrowed = "MATCH (p:Person {key: 'ben'})-[:BORROWED.a|BORROWED.b*2]->(t:Thing) RETURN t";
    assert_eq!(matches(&g, borrowed), set(&["laptop"]));

    // The loans are nodes, so their properties filter the way: only loans since 2025.
    let recent = "MATCH (p:Person)-[:BORROWED.a|BORROWED.b*2 (r, n | n.since >= '2025-01-01')]->(t:Thing) RETURN t";
    assert_eq!(
        matches(&g, recent),
        set(&["laptop"]),
        "only ben's loan is recent and leads to a thing"
    );

    let loans = "MATCH (p:Person)-[:BORROWED.a]->(l:Loan) WHERE l.since >= '2025-01-01' RETURN l";
    assert_eq!(matches(&g, loans), set(&["l2", "l3"]));

    // l3 names a camera that doesn't exist: its second half dangles.
    let second_half = report
        .link_types
        .iter()
        .find(|link_type| link_type.name == "BORROWED.b")
        .unwrap();

    assert_eq!(second_half.links, 2);
    assert_eq!(second_half.dangling, 1);
}
