# Rust library

The library has no HTTP in it. The server is a binary built on it, behind the `server` feature. It needs the nightly
toolchain pinned in `rust-toolchain.toml`.

```toml
[dependencies]
proximadb = { path = "../ProximaDB" }
```


## Load, write, read

This program runs on the files from the [quickstart](../README.md#quickstart), with their directory as its argument.

```rust
use std::collections::HashMap;
use std::path::PathBuf;

use proximadb::Store;
use proximadb::core::{Direction, Kind, Value};
use proximadb::load::{self, Backing, LinkTypeSpec, NodeTypeSpec, Schema};
use proximadb::query::condition::{self, Condition, Op};
use proximadb::query::cypher;
use proximadb::query::walk::{self, Reach, Through};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = PathBuf::from(std::env::args().nth(1).ok_or("give the data directory")?);

    let schema = Schema {
        node_types: vec![NodeTypeSpec {
            name: "Person".into(),
            file: "people.csv".into(),
            primary_key: "id".into(),
            properties: vec![("status".into(), Kind::String)],
        }],
        link_types: vec![LinkTypeSpec {
            name: "FOLLOWS".into(),
            from_type: "Person".into(),
            to_type: "Person".into(),
            backing: Backing::Dataset {
                file: "follows.csv".into(),
                from_column: "from".into(),
                to_column: "to".into(),
            },
        }],
    };

    let (graph, _report) = load::load_with_report(&dir, &schema)?;
    let store = Store::new(graph);

    let mut writer = store.writer();
    let person = writer.generation().graph.node_type("Person").ok_or("no Person")?;
    let follows = writer.generation().graph.link_type("FOLLOWS").ok_or("no FOLLOWS")?;
    let cleo = writer.generation().view().lookup(person, b"cleo").ok_or("no cleo")?;

    writer.begin(1);
    let dan = writer.add_node(person, b"dan");
    writer.set_property(dan, "status", &Value::Str("active".into()));
    writer.add_link(follows, cleo, dan);
    writer.commit();

    let generation = store.current();
    let view = generation.view();
    let ana = view.lookup(person, b"ana").ok_or("no ana")?;

    let active = Condition::compare("status", Op::Eq, Value::Str("active".into()));
    let active = condition::checked(view.graph, person, active)?;

    let mut through = Through::new(view.graph, false);
    through.set(person, condition::evaluate(&view, person, &active));

    let walked = walk::reach(
        &view,
        &Reach {
            starts: &[ana],
            link_types: &[follows],
            direction: Direction::Forward,
            max_steps: 3,
            through: Some(&through),
            cap: None,
        },
    );
    println!("reached {}", walked.reached.len());

    let query = "MATCH (a:Person {id: 'ana'})-[:FOLLOWS*1..3]->(b:Person) RETURN count(b)";
    let answer = cypher::run(&view, query, &HashMap::new()).map_err(|error| error.to_string())?;
    println!("count {}", answer.count.unwrap_or(0));

    Ok(())
}
```

It prints `reached 3` and `count 3`.


## The surface

| Module | Main items |
|---|---|
| `load` | `load`, `load_with_report`, `Schema`, `NodeTypeSpec`, `LinkTypeSpec`, `Backing`, `Side`, `Report` |
| `Store` | `new`, `with_limits`, `with_catch_up`, `current`, `writer`, `rebuilds` |
| `Writer` | `begin`, `add_node`, `set_property`, `delete_node`, `add_link`, `remove_link`, `commit`, `idle`, `rebuild_now`, `rebuild_and_wait`, `replace`, `make_current` |
| `View` | `lookup`, `key`, `get`, `type_of`, `count`, `nodes_of`, `is_deleted` |
| `query::condition` | `Condition`, `Op`, `checked`, `evaluate` |
| `query::walk` | `reach`, `first_step`, `path_length`, `Reach`, `Through` |
| `query::subgraph` | `neighbourhood`, `shortest_path`, `Subgraph`, `Role`, `LinkRef` |
| `query::cypher` | `run`, `Answer`, `CypherError` |
| `core` | `node::GlobalId`, `node::Id`, `node_type::Id`, `link_type::Id`, `link::Id`, `Value`, `Kind`, `Direction` |


## Rules the caller keeps

The library trusts its caller. The server checks these before calling it, and a release build aborts on a panic.

| Rule | Otherwise |
|---|---|
| one `Writer` at a time | `Store::writer` panics |
| `begin` gets a growing sequence number | panics |
| property names and value kinds match the schema | panics; `condition::checked` checks conditions |
| a link's ends have its link type's node types | `add_link` panics |
| a key is added once while its node exists | two nodes share the key |
| node numbers are used only with the generation they came from | they name other nodes: look up by key again after a commit |
| `idle` is called when no actions are waiting | a finished rebuild waits for the next commit |
| `make_current` follows `replace` | searches stay on the old graph, and no rebuild runs |
