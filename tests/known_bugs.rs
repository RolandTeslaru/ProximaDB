//! Bugs the tests found that aren't fixed yet. Each test states the correct behaviour, fails today, and is ignored so
//! that the suite stays usable. When one is fixed, delete its `ignore` and it guards against the bug coming back.
//!
//!     cargo test --release --features server --test known_bugs -- --ignored

mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use arrow_array::StringArray;
use common::{ask, column, drawn, node_type, people, set};
use proximadb::core::{Direction, Kind, Value};
use proximadb::load::{self, Schema};
use proximadb::query::condition::{self, Condition, Op};
use proximadb::query::cypher;
use proximadb::query::subgraph::Role;
use proximadb::query::walk::{self, Reach};
use proximadb::storage::node_type::Id as TypeId;
use proximadb::{Live, Store, View};

const PERSON: TypeId = TypeId(0);


fn matching(view: &View, property: &str, op: Op, value: Value) -> u64 {
    let condition = Condition::compare(property, op, value);
    let condition = condition::checked(view.graph, PERSON, condition).unwrap();

    condition::evaluate(view, PERSON, &condition).count()
}


// ---------------------------------------------------------------- crashes and races


#[test]
fn a_view_is_consistent_while_nodes_are_added() {
    let store = Store::with_limits(
        people("status", Kind::String, &[("ana", "on")], &[]),
        1.0,
        u64::MAX,
    );
    let done = AtomicBool::new(false);

    let torn = std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            while !done.load(Ordering::Relaxed) {
                let generation = store.current();
                let view = generation.view();

                if view.node_count() as u64 != view.count(PERSON) {
                    return true;
                }
            }

            false
        });

        let mut writer = store.writer();

        for seq in 1..=500_000u64 {
            if reader.is_finished() {
                break;
            }

            writer.begin(seq);
            writer.add_node(PERSON, &seq.to_le_bytes());
            writer.commit();
        }

        done.store(true, Ordering::Relaxed);

        reader.join().unwrap()
    });

    assert!(!torn, "node_count() and count() disagree within one view");
}


#[test]
fn filtering_while_nodes_are_added_doesnt_panic() {
    let store = Store::with_limits(
        people("status", Kind::String, &[("ana", "on")], &[]),
        1.0,
        u64::MAX,
    );
    let done = AtomicBool::new(false);

    let panicked = std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            while !done.load(Ordering::Relaxed) {
                let generation = store.current();
                let view = generation.view();

                // Only a view caught between an action's two publishing stores.
                if view.node_count() as u64 == view.count(PERSON) {
                    continue;
                }

                let query = "MATCH (a:Person {status: 'on'}) RETURN count(a)";
                let run = std::panic::AssertUnwindSafe(|| {
                    cypher::run(&view, query, &HashMap::new()).is_ok()
                });

                return std::panic::catch_unwind(run).is_err();
            }

            false
        });

        let mut writer = store.writer();

        for seq in 1..=50_000u64 {
            if reader.is_finished() {
                break;
            }

            writer.begin(seq);

            // A whole bitset word of nodes in each action.
            for i in 0..64u64 {
                writer.add_node(PERSON, &(seq * 64 + i).to_le_bytes());
            }

            writer.commit();
        }

        done.store(true, Ordering::Relaxed);

        reader.join().unwrap()
    });

    assert!(!panicked, "the query panicked");
}


// ---------------------------------------------------------------- sequence numbers


#[test]
fn a_rebuild_never_moves_the_sequence_back() {
    let store = Store::new(people("status", Kind::String, &[("ana", "on")], &[]));
    let mut writer = store.writer();

    writer.rebuild_now();
    writer.begin(1);
    writer.commit();
    writer.finish_rebuild();

    assert_eq!(store.current().view().upto, 1);
}


#[test]
fn the_largest_sequence_number_works() {
    let store = Store::new(people(
        "status",
        Kind::String,
        &[("ana", "on"), ("ben", "on")],
        &[],
    ));
    let mut writer = store.writer();

    let (ana, ben, follows) = {
        let generation = writer.generation();
        let view = generation.view();

        (
            view.lookup(PERSON, b"ana").unwrap(),
            view.lookup(PERSON, b"ben").unwrap(),
            view.graph.link_type("FOLLOWS").unwrap(),
        )
    };

    writer.begin(u64::MAX);
    writer.add_link(follows, ana, ben);
    writer.commit();

    let generation = store.current();
    let walked = walk::reach(
        &generation.view(),
        &Reach {
            starts: &[ana],
            link_types: &[follows],
            direction: Direction::Forward,
            max_steps: 1,
            through: None,
            cap: None,
        },
    );

    assert_eq!(walked.reached, [ben]);
}


// ---------------------------------------------------------------- values


#[test]
#[ignore = "bug: integers above 2^53 in Cypher name a different node"]
fn a_large_integer_key_finds_its_own_node() {
    let rows = [("9007199254740992", "even"), ("9007199254740993", "odd")];
    let graph = people("parity", Kind::String, &rows, &[]);
    let live = Live::new(&graph);
    let view = live.view(&graph);

    let found = cypher::run(
        &view,
        "MATCH (a:Person {key: 9007199254740993}) RETURN a",
        &HashMap::new(),
    );

    // Refusing the number would also be correct; answering with the neighbouring key is not.
    if let Ok(answer) = found {
        assert_eq!(
            drawn(&view, &answer, Role::Match),
            set(&["9007199254740993"])
        );
    }
}


#[test]
#[ignore = "bug: long values above 2^53 compare equal to their neighbours"]
fn long_values_compare_exactly() {
    let rows = [("a", "9007199254740992"), ("b", "9007199254740993")];
    let graph = people("n", Kind::Long, &rows, &[]);
    let live = Live::new(&graph);

    assert_eq!(
        matching(
            &live.view(&graph),
            "n",
            Op::Eq,
            Value::Num(9_007_199_254_740_992.0)
        ),
        1
    );
}


#[test]
#[ignore = "bug: an integer beyond 32 bits wraps round when loaded"]
fn a_loaded_integer_doesnt_wrap() {
    let graph = people("n", Kind::Integer, &[("ana", "3000000000")], &[]);
    let live = Live::new(&graph);

    assert_eq!(
        matching(&live.view(&graph), "n", Op::Lt, Value::Num(0.0)),
        0
    );
}


#[test]
#[ignore = "bug: an integer written beyond 32 bits changes at the next rebuild"]
fn a_written_integer_survives_a_rebuild() {
    let store = Store::new(people("n", Kind::Integer, &[("ana", "1")], &[]));
    let mut writer = store.writer();
    let ana = writer.generation().view().lookup(PERSON, b"ana").unwrap();

    writer.begin(1);
    writer.set_property(ana, "n", &Value::Num(3_000_000_000.0));
    writer.commit();

    let before = matching(&store.current().view(), "n", Op::Gt, Value::Num(0.0));
    writer.rebuild_and_wait();

    assert_eq!(
        matching(&store.current().view(), "n", Op::Gt, Value::Num(0.0)),
        before
    );
}


#[test]
#[ignore = "bug: NaN answers <> differently once the node is touched"]
fn nan_compares_the_same_packed_and_changed() {
    let store = Store::new(people("x", Kind::Double, &[("ana", "NaN")], &[]));
    let before = matching(&store.current().view(), "x", Op::Ne, Value::Num(5.0));

    let mut writer = store.writer();
    let ana = writer.generation().view().lookup(PERSON, b"ana").unwrap();

    writer.begin(1);
    writer.set_property(ana, "x", &Value::Num(f64::NAN));
    writer.commit();

    assert_eq!(
        matching(&store.current().view(), "x", Op::Ne, Value::Num(5.0)),
        before
    );
}


#[test]
#[ignore = "bug: an empty string in Parquet loads as no value"]
fn parquet_empty_string_is_a_value() {
    let dir = common::Dir::new();
    dir.parquet(
        "people.parquet",
        vec![
            ("key", column(StringArray::from(vec!["ana", "ben"]))),
            ("note", column(StringArray::from(vec![Some(""), None]))),
        ],
    );

    let schema = Schema {
        node_types: vec![node_type(
            "Person",
            "people.parquet",
            &[("note", Kind::String)],
        )],
        link_types: vec![],
    };
    let graph = load::load(dir.path(), &schema).unwrap();

    assert_eq!(graph.property(PERSON, "note").unwrap().has_value.count(), 1);
}


#[test]
#[ignore = "bug: a Parquet column with a dot in its name can't be read"]
fn parquet_columns_with_dots() {
    let dir = common::Dir::new();
    dir.parquet(
        "people.parquet",
        vec![
            ("key", column(StringArray::from(vec!["ana"]))),
            ("address.city", column(StringArray::from(vec!["Cluj"]))),
        ],
    );

    let schema = Schema {
        node_types: vec![node_type(
            "Person",
            "people.parquet",
            &[("address.city", Kind::String)],
        )],
        link_types: vec![],
    };
    let loaded = load::load(dir.path(), &schema);

    assert!(loaded.is_ok(), "{:?}", loaded.err());
}


#[test]
#[ignore = "bug: dates that don't exist are accepted"]
fn impossible_dates_are_refused() {
    for text in [
        "2025-02-30",
        "2025-13-01",
        "2025-00-10",
        "2025-04-31",
        "2025-01-01junk",
    ] {
        assert_eq!(
            proximadb::storage::property::parse_date(text),
            None,
            "{text}"
        );
    }
}


#[test]
#[ignore = "bug: a timestamp's time zone offset is ignored"]
fn timestamp_offsets_are_applied() {
    let rows = [
        ("utc", "2026-01-01T08:00:00Z"),
        ("plus2", "2026-01-01T10:00:00+02:00"),
    ];
    let graph = people("at", Kind::Timestamp, &rows, &[]);
    let live = Live::new(&graph);
    let eight_utc = Value::Num(1_767_254_400_000_000.0);

    assert_eq!(matching(&live.view(&graph), "at", Op::Eq, eight_utc), 2);
}


#[test]
#[ignore = "bug: whether strings can be ordered depends on how many distinct values the data has"]
fn string_ordering_doesnt_depend_on_the_data() {
    let distinct = people(
        "name",
        Kind::String,
        &[("1", "a"), ("2", "b"), ("3", "c")],
        &[],
    );
    let repeated = people(
        "name",
        Kind::String,
        &[("1", "a"), ("2", "a"), ("3", "a")],
        &[],
    );

    let allowed = |graph| {
        let condition = Condition::compare("name", Op::Lt, Value::Str("m".into()));

        condition::checked(graph, PERSON, condition).is_ok()
    };

    assert_eq!(allowed(&distinct), allowed(&repeated));
}


// ---------------------------------------------------------------- Cypher


#[test]
#[ignore = "bug: one name for both ends is treated as two nodes"]
fn one_name_means_one_node() {
    let rows = [("ana", "on"), ("ben", "on")];
    let graph = people("status", Kind::String, &rows, &[("ana", "ben")]);
    let live = Live::new(&graph);

    let query = "MATCH (a:Person {key: 'ana'})-[:FOLLOWS]->(a:Person) RETURN a";

    // ana doesn't follow herself: zero, or an error saying this isn't supported.
    if let Ok(answer) = cypher::run(&live.view(&graph), query, &HashMap::new()) {
        assert_eq!(answer.count, Some(0));
    }
}


#[test]
#[ignore = "bug: round a loop, the drawing loses its start"]
fn a_loop_keeps_its_start() {
    let rows = [("ana", "on"), ("ben", "on")];
    let graph = people(
        "status",
        Kind::String,
        &rows,
        &[("ana", "ben"), ("ben", "ana")],
    );
    let live = Live::new(&graph);
    let view = live.view(&graph);

    let answer = ask(
        &view,
        "MATCH (a:Person {key: 'ana'})-[:FOLLOWS*1..2]->(b:Person) RETURN a, b",
    );

    assert_eq!(drawn(&view, &answer, Role::Start), set(&["ana"]));
}


#[test]
#[ignore = "bug: with *0.. a start is drawn twice against LIMIT"]
fn limit_counts_each_match_once() {
    let rows = [("ana", "on"), ("ben", "on"), ("cleo", "on")];
    let follows = [
        ("ana", "ben"),
        ("ben", "ana"),
        ("ana", "ana"),
        ("ben", "cleo"),
    ];
    let graph = people("status", Kind::String, &rows, &follows);
    let live = Live::new(&graph);
    let view = live.view(&graph);

    let answer = ask(
        &view,
        "MATCH (a:Person {key: 'ana'})-[:FOLLOWS*0..3]->(b:Person) RETURN b LIMIT 3",
    );

    assert_eq!(drawn(&view, &answer, Role::Match).len(), 3);
}


#[test]
#[ignore = "bug: undirected, a node is its own match by going back over the same link"]
fn a_link_isnt_walked_back_over() {
    let rows = [("ana", "on"), ("ben", "on")];
    let graph = people("status", Kind::String, &rows, &[("ana", "ben")]);
    let live = Live::new(&graph);

    let answer = ask(
        &live.view(&graph),
        "MATCH (a:Person {key: 'ana'})-[:FOLLOWS*1..2]-(b:Person) RETURN count(b)",
    );

    assert_eq!(
        answer.count,
        Some(1),
        "only ben: ana is no match of her own"
    );
}


// ---------------------------------------------------------------- the server


#[cfg(feature = "server")]
mod server {
    use std::time::Duration;

    use serde_json::json;

    use super::common::Dir;
    use super::common::server::{Server, binary, person};


    #[test]
    fn a_reload_survives_its_client_leaving() {
        let dir = Dir::new();

        // Big enough that the load is still running when the client leaves.
        let mut people = String::from("key\n");
        (0..300_000).for_each(|i| people += &format!("p{i}\n"));
        dir.write("people.csv", people);

        let schema = json!({
            "node_types": [{"name": "Person", "file": "people.csv", "primary_key": "key", "properties": {}}],
            "link_types": []
        });
        let server = Server::start(dir, schema);

        let mut leaving = server.client();
        leaving.send(
            "POST",
            "/reload",
            Some("application/json"),
            br#"{"as_of_seq":0}"#,
        );
        std::thread::sleep(Duration::from_millis(30));
        drop(leaving);

        std::thread::sleep(Duration::from_secs(3));

        let response = server.client().post("/reload", json!({"as_of_seq": 0}));
        assert_eq!(response.status, 200, "{}", response.body);
    }


    #[test]
    fn a_failed_replay_keeps_the_sequence() {
        let server = Server::small();
        let mut client = server.client();

        client
            .post(
                "/actions",
                json!({"seq": 1, "edits": [{"op": "add_node", "type": "Person", "key": "fay"}]}),
            )
            .ok();
        client
            .post(
                "/actions",
                json!({"seq": 2, "edits": [{"op": "delete_node", "node": person("fay")}]}),
            )
            .ok();

        let reloaded = client.post("/reload", json!({"as_of_seq": 1})).ok();

        assert_eq!(reloaded["seq"], 2);
    }


    #[test]
    fn reach_applies_both_filters() {
        let server = Server::small();

        let body = json!({
            "start": [person("ana")], "link_types": ["FOLLOWS"], "direction": "forward", "max_steps": 1,
            "through": [
                {"type": "Person", "where": {"property": "age", "op": "gt", "value": 40}},
                {"type": "Person", "where": {"property": "status", "op": "ne", "value": "banned"}}
            ]
        });

        assert_eq!(server.client().post("/reach", body).ok()["count"], 1);
    }


    #[test]
    fn an_empty_key_is_refused() {
        let server = Server::small();
        let body = json!({"seq": 1, "edits": [{"op": "add_node", "type": "Person", "key": ""}]});

        assert_eq!(server.client().post("/actions", body).status, 400);
    }


    #[test]
    fn an_action_can_replace_an_object() {
        let server = Server::small();
        let body = json!({"seq": 1, "edits": [
            {"op": "delete_node", "node": person("ana")},
            {"op": "add_node", "type": "Person", "key": "ana"}
        ]});

        let response = server.client().post("/actions", body);
        assert_eq!(response.status, 200, "{}", response.body);
    }


    #[test]
    fn a_link_to_a_node_deleted_in_the_same_action_is_refused() {
        let server = Server::small();
        let body = json!({"seq": 1, "edits": [
            {"op": "delete_node", "node": person("ben")},
            {"op": "add_link", "link_type": "FOLLOWS", "from": person("eve"), "to": person("ben")}
        ]});

        assert_eq!(server.client().post("/actions", body).status, 400);
    }


    #[test]
    #[ignore = "bug: --unix deletes whatever file is at that path"]
    fn the_socket_flag_deletes_only_sockets() {
        // Everything here is inside the test's own directory.
        let dir = Dir::new();
        dir.write("precious.txt", "keep me");
        dir.write("schema.json", r#"{"node_types":[],"link_types":[]}"#);

        let path = dir.path().join("precious.txt");
        let mut process = std::process::Command::new(binary())
            .arg("--schema")
            .arg(dir.path().join("schema.json"))
            .arg("--data")
            .arg(dir.path())
            .arg("--unix")
            .arg(&path)
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();

        std::thread::sleep(Duration::from_secs(1));
        let _ = process.kill();
        process.wait().unwrap();

        assert_eq!(std::fs::read(&path).ok().as_deref(), Some(&b"keep me"[..]));
    }
}
