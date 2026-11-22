//! How the big walks scale with threads, and how much memory traffic they generate, to see whether they hit the
//! machine's memory limits or its CPU limits.
//!
//!     cargo run --release --bin scaling -- <dataset dir>

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use proximadb::Graph;
use proximadb::core::Direction;
use proximadb::load;
use proximadb::query::condition::{self, Condition, Op, Value};
use proximadb::query::walk::{self, Reach, ReachResult, Through};
use proximadb::core::node::GlobalId;
use serde_json::Value as Json;

#[path = "shared/dataset.rs"]
mod dataset;
use dataset::schema;

fn global(graph: &Graph, record: &Json) -> GlobalId {
    let t = graph.node_type(record["label"].as_str().unwrap()).unwrap();
    let key = record["nodeId"].as_u64().unwrap().to_string();
    graph
        .key_index
        .get(&graph.keys, &graph.node_type_mappings, t, key.as_bytes())
        .unwrap()
}

fn pool(threads: usize) -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .unwrap()
}

#[derive(Default, Clone, Copy)]
struct Work {
    scanned: u64,
    reached: u64,
}

impl Work {
    fn add(&mut self, r: &ReachResult) {
        self.scanned += r.scanned;
        self.reached += r.reached.len() as u64;
    }
}

/// Estimated random 64-byte lines read from memory: for each node expanded, its type, its "starts at" entries and the
/// start of its part of "points to" (3); with a path filter, each connection checked also reads the filter bit of the
/// node found (1). The node found's type comes from the link type, so it costs no read.
fn lines(w: Work, filtered: bool) -> f64 {
    3.0 * w.reached as f64 + if filtered { w.scanned as f64 } else { 0.0 }
}

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).unwrap_or("research/data/M".into()));
    let graph = load::load(&dir, &schema()).expect("load");
    let live = proximadb::Live::new(&graph);
    let view = live.view(&graph);
    let params: Json =
        serde_json::from_str(&std::fs::read_to_string(dir.join("params.json")).unwrap()).unwrap();
    let starts: Vec<GlobalId> = params["followers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| global(&graph, r))
        .collect();
    let user = graph.node_type("User").unwrap();
    let follows = [graph.link_type("FOLLOWS").unwrap()];
    let all_rel = graph.all_link_types();
    let mut through = Through::new(&graph, false);
    through.set(
        user,
        condition::evaluate_packed(
            &graph,
            user,
            &Condition::compare("status", Op::Eq, Value::Str("active".into())),
        ),
    );
    let banned = condition::evaluate_packed(
        &graph,
        user,
        &Condition::compare("banned", Op::Eq, Value::Bool(true)),
    );
    let banned_starts: Vec<GlobalId> = banned.ones().map(|l| GlobalId(l as u64)).collect();

    let max_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let mut counts = vec![1, 2, 4, 8];
    if max_threads > 8 {
        counts.push(max_threads);
    }

    let walk4 = |s: GlobalId, filtered: bool| {
        walk::reach(
            &view,
            &Reach {
                starts: &[s],
                link_types: &follows,
                direction: Direction::Forward,
                max_steps: 4,
                through: filtered.then_some(&through),
                cap: None,
            },
        )
    };

    println!("# Thread scaling, {} nodes\n", graph.node_count());
    println!(
        "Per query: connections read and nodes reached, averaged over the {} starts.\n",
        starts.len()
    );

    // ---- one query at a time, split across t threads ----
    for (name, filtered) in [
        ("within 4 follows, no filter", false),
        ("within 4 follows, only through active users", true),
    ] {
        println!("## One query at a time: {name}\n");
        println!(
            "| Threads | ms per query | M connections/s | ns per connection | est. GB/s random lines |\n|---|---|---|---|---|"
        );
        for &t in &counts {
            let p = pool(t);
            p.install(|| {
                for &s in &starts[..10] {
                    walk4(s, filtered); // warm-up
                }
                let mut w = Work::default();
                let clock = Instant::now();
                for &s in &starts {
                    w.add(&walk4(s, filtered));
                }
                let secs = clock.elapsed().as_secs_f64();
                println!(
                    "| {t} | {:.2} | {:.0} | {:.2} | {:.1} |",
                    secs * 1000.0 / starts.len() as f64,
                    w.scanned as f64 / secs / 1e6,
                    secs * 1e9 / w.scanned as f64,
                    lines(w, filtered) * 64.0 / secs / 1e9
                );
            });
        }
        println!();
    }

    // ---- t queries at once, one thread each ----
    for (name, filtered) in [
        ("within 4 follows, no filter", false),
        ("within 4 follows, only through active users", true),
    ] {
        println!("## Many queries at once, one thread each: {name}\n");
        println!(
            "| Queries at once | queries/s | M connections/s | est. GB/s random lines |\n|---|---|---|---|"
        );
        for &t in &counts {
            let next = AtomicUsize::new(0);
            let total = starts.len() * 2;
            let clock = Instant::now();
            let work: Vec<Work> = std::thread::scope(|scope| {
                let handles: Vec<_> = (0..t)
                    .map(|_| {
                        scope.spawn(|| {
                            let p = pool(1);
                            p.install(|| {
                                let mut w = Work::default();
                                loop {
                                    let i = next.fetch_add(1, Ordering::Relaxed);
                                    if i >= total {
                                        break w;
                                    }
                                    w.add(&walk4(starts[i % starts.len()], filtered));
                                }
                            })
                        })
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
            let secs = clock.elapsed().as_secs_f64();
            let w = work.iter().fold(Work::default(), |a, b| Work {
                scanned: a.scanned + b.scanned,
                reached: a.reached + b.reached,
            });
            println!(
                "| {t} | {:.0} | {:.0} | {:.1} |",
                total as f64 / secs,
                w.scanned as f64 / secs / 1e6,
                lines(w, filtered) * 64.0 / secs / 1e9
            );
        }
        println!();
    }

    // ---- whole graph ----
    println!("## Whole graph: distance to the nearest banned user\n");
    println!("| Threads | ms | M connections/s |\n|---|---|---|");
    for &t in &counts {
        let p = pool(t);
        p.install(|| {
            let run = || {
                walk::reach(
                    &view,
                    &Reach {
                        starts: &banned_starts,
                        link_types: &all_rel,
                        direction: Direction::Backward,
                        max_steps: 4,
                        through: None,
                        cap: None,
                    },
                )
            };
            run();
            let clock = Instant::now();
            let r = run();
            let secs = clock.elapsed().as_secs_f64();
            println!(
                "| {t} | {:.1} | {:.0} |",
                secs * 1000.0,
                r.scanned as f64 / secs / 1e6
            );
        });
    }
}
