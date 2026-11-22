//! Performance on a generated graph, so it runs anywhere without the research data. Ignored by default:
//!
//!     cargo test --release --test perf -- --ignored --nocapture
//!
//! Each measurement prints, and fails beyond `SLACK` times its budget. A budget is about twice what an Apple M3 Pro
//! measures (load about half its budget), so a failure is a regression of several times, not noise. On a slower
//! machine, raise `SLACK`.

mod common;

use std::time::Instant;

use common::{Dir, Rng, ask, link_type, node_type};
use proximadb::Store;
use proximadb::core::{Direction, Kind, Value};
use proximadb::load::{self, Schema};
use proximadb::query::condition::{self, Condition, Op};
use proximadb::query::walk::{self, Reach, Through};
use proximadb::storage::node_type::Id as TypeId;

const PEOPLE: usize = 500_000;
const FOLLOWS_EACH: usize = 20;
const SLACK: f64 = 3.0;


fn generate(dir: &Dir) -> Schema {
    let mut rng = Rng(5);
    let mut people = String::from("key,status,spent\n");
    let mut follows = String::from("a,b\n");

    for i in 0..PEOPLE {
        let status = ["active", "inactive"][rng.below(2)];
        people += &format!("p{i},{status},{}\n", rng.below(1_000));

        for _ in 0..FOLLOWS_EACH {
            // A few hubs that many follow, as in social graphs.
            let to = if rng.chance(10) {
                rng.below(100)
            } else {
                rng.below(PEOPLE)
            };
            follows += &format!("p{i},p{to}\n");
        }
    }

    dir.write("people.csv", people);
    dir.write("follows.csv", follows);

    Schema {
        node_types: vec![node_type(
            "Person",
            "people.csv",
            &[("status", Kind::String), ("spent", Kind::Double)],
        )],
        link_types: vec![link_type("FOLLOWS", "Person", "Person", "follows.csv")],
    }
}


fn measure<T>(name: &str, budget_ms: f64, runs: usize, mut work: impl FnMut() -> T) -> T {
    let mut times = Vec::with_capacity(runs);
    let mut last = None;

    for _ in 0..runs {
        let started = Instant::now();
        last = Some(std::hint::black_box(work()));
        times.push(started.elapsed().as_secs_f64() * 1000.0);
    }

    times.sort_by(f64::total_cmp);
    let median = times[times.len() / 2];

    println!("| {name} | {median:.2} ms | {budget_ms} ms |");
    assert!(
        median <= budget_ms * SLACK,
        "{name}: {median:.2} ms, budget {budget_ms} ms"
    );

    last.unwrap()
}


#[test]
#[ignore = "slow: run with --ignored --nocapture"]
fn generated_graph() {
    let dir = Dir::new();
    let schema = generate(&dir);

    println!("\n| Measure | Median | Budget |\n|---|---|---|");

    let graph = measure("load 0.5 M people, 10 M follows", 3_000.0, 1, || {
        load::load(dir.path(), &schema).unwrap()
    });
    let store = Store::new(graph);
    let generation = store.current();
    let view = generation.view();

    let person = TypeId(0);
    let follows = [view.graph.link_type("FOLLOWS").unwrap()];
    let ordinary = view.lookup(person, b"p250000").unwrap();
    let hub = view.lookup(person, b"p1").unwrap();

    let active = Condition::compare("status", Op::Eq, Value::Str("active".into()));
    let bits = measure("condition over every person", 0.05, 20, || {
        condition::evaluate(&view, person, &active)
    });

    let mut through = Through::new(view.graph, false);
    through.set(person, bits);

    for (name, start, budget) in [("ordinary", ordinary, 0.4), ("hub", hub, 0.4)] {
        let query = Reach {
            starts: &[start],
            link_types: &follows,
            direction: Direction::Forward,
            max_steps: 4,
            through: Some(&through),
            cap: None,
        };

        let walked = measure(
            &format!("4 steps through active people, {name} start"),
            budget,
            15,
            || walk::reach(&view, &query),
        );

        println!("|   reached | {} nodes | |", walked.reached.len());
        assert!(
            walked.reached.len() > 1_000,
            "{name}: the walk should reach far"
        );
    }

    measure("shortest path", 0.15, 50, || {
        walk::path_length(&view, ordinary, hub, &follows, Direction::Forward, 6, None)
    });

    measure("Cypher, filtered walk with a graph of 500", 0.5, 10, || {
        ask(
            &view,
            "MATCH (a:Person {key: 'p250000'})-[:FOLLOWS*1..3 (r, n | n.status = 'active')]->(b:Person) \
             WHERE b.spent >= 500 RETURN b LIMIT 500",
        )
    });

    measure("Cypher, every active person's follows", 16.0, 5, || {
        ask(
            &view,
            "MATCH (a:Person {status: 'active'})-[:FOLLOWS]->(b:Person) RETURN count(b)",
        )
    });

    drop(generation);

    let mut writer = store.writer();
    let mut seq = 0;

    measure("10,000 actions, one link each", 5.0, 3, || {
        let (people, follows) = {
            let view = writer.generation().view();
            let people: Vec<_> = (0..10_000)
                .map(|i| view.lookup(person, format!("p{i}").as_bytes()).unwrap())
                .collect();

            (people, view.graph.link_type("FOLLOWS").unwrap())
        };

        for pair in people.windows(2) {
            seq += 1;
            writer.begin(seq);
            writer.add_link(follows, pair[0], pair[1]);
            writer.commit();
        }
    });

    measure("rebuild", 600.0, 1, || writer.rebuild_and_wait());
}
