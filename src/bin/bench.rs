//! Loads the social-network test dataset, checks every answer against the expected ones, and times each query on
//! one thread and on all threads.
//!
//!     cargo run --release --bin bench -- <dataset dir> [results.json]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use proximadb::Graph;
use proximadb::core::Direction;
use proximadb::load;
use proximadb::query::condition::{self, Condition, Op, Value};

#[path = "shared/dataset.rs"]
mod dataset;
use dataset::{peak_rss_bytes, schema};
use proximadb::core::node::GlobalId;
use proximadb::query::walk::{self, Reach, Through};
use rayon::ThreadPool;
use serde_json::{Value as Json, json};

const RUNS: usize = 3;

fn global(graph: &Graph, record: &Json) -> GlobalId {
    let t = graph.node_type(record["label"].as_str().unwrap()).unwrap();
    let key = record["nodeId"].as_u64().unwrap().to_string();
    graph
        .key_index
        .get(&graph.keys, &graph.node_type_mappings, t, key.as_bytes())
        .expect("seed")
}

/// One measured case: a group of starts, a function giving the answer for one start, and the expected answers.
struct Case {
    query: String,
    group: String,
    inputs: Vec<usize>,
}

struct Timing {
    median_ms: f64,
    p95_ms: f64,
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i]
}

fn time_case(pool: &ThreadPool, case: &Case, run: &(dyn Fn(usize) -> Json + Sync)) -> Timing {
    pool.install(|| {
        let mut per_input: Vec<f64> = Vec::new();
        for &i in &case.inputs {
            let _ = run(i); // warm-up
            let mut runs: Vec<f64> = (0..RUNS)
                .map(|_| {
                    let t = Instant::now();
                    std::hint::black_box(run(i));
                    t.elapsed().as_secs_f64() * 1000.0
                })
                .collect();
            runs.sort_by(f64::total_cmp);
            per_input.push(runs[RUNS / 2]);
        }
        per_input.sort_by(f64::total_cmp);
        Timing {
            median_ms: percentile(&per_input, 0.5),
            p95_ms: percentile(&per_input, 0.95),
        }
    })
}

fn fmt(ms: f64) -> String {
    if ms >= 100.0 {
        format!("{ms:.0}")
    } else if ms >= 1.0 {
        format!("{ms:.2}")
    } else {
        format!("{ms:.3}")
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = PathBuf::from(args.get(1).map(String::as_str).unwrap_or("research/data/S"));
    let out_path = args.get(2).map(PathBuf::from);

    let started = Instant::now();
    let graph = load::load(&dir, &schema()).expect("load");
    let load_s = started.elapsed().as_secs_f64();
    let peak = peak_rss_bytes();

    let draft = proximadb::Draft::new(&graph);
    let view = draft.view(&graph);
    let params: Json =
        serde_json::from_str(&std::fs::read_to_string(dir.join("params.json")).unwrap()).unwrap();
    let expected: Json =
        serde_json::from_str(&std::fs::read_to_string(dir.join("expected.json")).unwrap()).unwrap();

    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let one = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .unwrap();
    let all = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .unwrap();

    println!("# ProximaDB layout test\n");
    println!(
        "{} nodes, {} links, loaded in {load_s:.2} s; peak process memory {:.0} MB\n",
        graph.node_count(),
        graph
            .link_types
            .iter()
            .map(|r| r.forward.len())
            .sum::<usize>(),
        peak as f64 / 1e6
    );
    println!("| Structure | MB |\n|---|---|");
    let mut total = 0;
    for (name, bytes) in graph.memory() {
        total += bytes;
        println!("| {name} | {:.1} |", bytes as f64 / 1e6);
    }
    println!("| **total** | **{:.1}** |\n", total as f64 / 1e6);

    let user = graph.node_type("User").unwrap();
    let follows = [graph.link_type("FOLLOWS").unwrap()];
    let all_rel = graph.all_link_types();
    let status_active = Condition::compare("status", Op::Eq, Value::Str("active".into()));

    // ---- inputs ----
    let general: Vec<(GlobalId, bool)> = params["general"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (global(&graph, r), r["hub"].as_bool().unwrap()))
        .collect();
    let followers: Vec<(GlobalId, bool, String)> = params["followers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                global(&graph, r),
                r["hub"].as_bool().unwrap(),
                r["language"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let pairs: Vec<(GlobalId, GlobalId)> = params["pairs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (global(&graph, &p["from"]), global(&graph, &p["to"])))
        .collect();
    let long_pairs: Vec<(GlobalId, GlobalId, String)> = params["long_pairs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                global(&graph, &p["from"]),
                global(&graph, &p["to"]),
                p["group"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let loops: Vec<(GlobalId, String)> = params["loops"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (global(&graph, r), r["group"].as_str().unwrap().to_string()))
        .collect();

    // ---- queries: answer for input i ----
    let reach_count = |start: GlobalId, direction: Direction, steps: u32| -> Json {
        let r = walk::reach(
            &view,
            &Reach {
                starts: &[start],
                link_types: &all_rel,
                direction,
                max_steps: steps,
                through: None,
                cap: Some(10_000),
            },
        );
        json!((r.reached.len() as u64).min(10_000))
    };
    let count_where = |reached: &[GlobalId], allowed: &proximadb::Bitset| -> u64 {
        reached
            .iter()
            .filter(|&&g| {
                let (t, local) = graph.node_type_mappings.get(g);
                t == user && allowed.get(local.index())
            })
            .count() as u64
    };

    type Query<'a> = Box<dyn Fn(usize) -> Json + Sync + 'a>;
    let mut queries: Vec<(String, Query, Vec<Json>, Vec<String>)> = Vec::new();
    let hub_groups = |list: &[bool]| {
        list.iter()
            .map(|&h| {
                if h {
                    "hub".to_string()
                } else {
                    "ordinary".to_string()
                }
            })
            .collect::<Vec<_>>()
    };
    let general_groups = hub_groups(&general.iter().map(|g| g.1).collect::<Vec<_>>());
    let follower_groups = hub_groups(&followers.iter().map(|f| f.1).collect::<Vec<_>>());
    let arr = |k: &str| expected[k].as_array().unwrap().clone();

    queries.push((
        "Q4 within 3 steps, any link type (cap 10,000)".into(),
        Box::new(|i| reach_count(general[i].0, Direction::Forward, 3)),
        arr("Q4.out.3"),
        general_groups.clone(),
    ));
    queries.push((
        "Q4 within 5 steps, any link type (cap 10,000)".into(),
        Box::new(|i| reach_count(general[i].0, Direction::Forward, 5)),
        arr("Q4.out.5"),
        general_groups.clone(),
    ));
    queries.push((
        "Q4r reaching this one within 5 steps (cap 10,000)".into(),
        Box::new(|i| reach_count(general[i].0, Direction::Backward, 5)),
        arr("Q4.in.5"),
        general_groups.clone(),
    ));
    queries.push((
        "Q11 end filter: within 4 follows, active + same language".into(),
        Box::new(|i| {
            let (start, _, lang) = &followers[i];
            let cond = Condition::And(vec![
                status_active.clone(),
                Condition::compare("language", Op::Eq, Value::Str(lang.clone())),
            ]);
            let allowed = condition::evaluate_packed(&graph, user, &cond);
            let r = walk::reach(
                &view,
                &Reach {
                    starts: &[*start],
                    link_types: &follows,
                    direction: Direction::Forward,
                    max_steps: 4,
                    through: None,
                    cap: None,
                },
            );
            json!(count_where(&r.reached, &allowed))
        }),
        arr("Q11"),
        follower_groups.clone(),
    ));
    queries.push((
        "Q12 path filter: within 4 follows, only through active users".into(),
        Box::new(|i| {
            let mut through = Through::new(&graph, false);
            through.set(
                user,
                condition::evaluate_packed(&graph, user, &status_active),
            );
            let r = walk::reach(
                &view,
                &Reach {
                    starts: &[followers[i].0],
                    link_types: &follows,
                    direction: Direction::Forward,
                    max_steps: 4,
                    through: Some(&through),
                    cap: None,
                },
            );
            json!(r.reached.len())
        }),
        arr("Q12"),
        follower_groups.clone(),
    ));
    queries.push((
        "Q13 range filter: within 4 follows, spent 100–500".into(),
        Box::new(|i| {
            let cond = Condition::And(vec![
                Condition::compare("spent", Op::Ge, Value::Num(100.0)),
                Condition::compare("spent", Op::Lt, Value::Num(500.0)),
            ]);
            let allowed = condition::evaluate_packed(&graph, user, &cond);
            let r = walk::reach(
                &view,
                &Reach {
                    starts: &[followers[i].0],
                    link_types: &follows,
                    direction: Direction::Forward,
                    max_steps: 4,
                    through: None,
                    cap: None,
                },
            );
            json!(count_where(&r.reached, &allowed))
        }),
        arr("Q13"),
        follower_groups.clone(),
    ));
    queries.push((
        "Q5 shortest path, any link type (up to 6)".into(),
        Box::new(|i| {
            json!(walk::path_length(
                &view,
                pairs[i].0,
                pairs[i].1,
                &all_rel,
                Direction::Forward,
                6,
                None
            ))
        }),
        arr("Q5"),
        vec!["all pairs".to_string(); pairs.len()],
    ));
    queries.push((
        "Q9 long path, any link type (up to 1,100)".into(),
        Box::new(|i| {
            json!(walk::path_length(
                &view,
                long_pairs[i].0,
                long_pairs[i].1,
                &all_rel,
                Direction::Forward,
                1100,
                None
            ))
        }),
        arr("Q9"),
        long_pairs.iter().map(|p| p.2.clone()).collect(),
    ));
    queries.push((
        "Q10 follow loop (up to 1,100)".into(),
        Box::new(|i| {
            json!(walk::path_length(
                &view,
                loops[i].0,
                loops[i].0,
                &follows,
                Direction::Forward,
                1100,
                None
            ))
        }),
        arr("Q10"),
        loops.iter().map(|l| l.1.clone()).collect(),
    ));

    println!("## Per-start queries (median / p95 ms, in-process, no network)\n");
    println!(
        "| Query | Group | Starts | Correct | 1 thread | {threads} threads |\n|---|---|---|---|---|---|"
    );
    let mut results = Vec::new();
    let mut wrong_total = 0;
    for (name, run, want, groups) in &queries {
        // correctness, on all threads
        let got: Vec<Json> = all.install(|| (0..want.len()).map(run).collect());
        let mut by_group: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        for (i, g) in groups.iter().enumerate() {
            by_group.entry(g.as_str()).or_default().push(i);
        }
        for (group, inputs) in by_group {
            let ok = inputs.iter().filter(|&&i| got[i] == want[i]).count();
            for &i in &inputs {
                if got[i] != want[i] && wrong_total < 10 {
                    eprintln!(
                        "WRONG {name} [{group}] input {i}: got {} expected {}",
                        got[i], want[i]
                    );
                }
            }
            wrong_total += inputs.len() - ok;
            let case = Case {
                query: name.clone(),
                group: group.to_string(),
                inputs,
            };
            let t1 = time_case(&one, &case, run.as_ref());
            let tn = time_case(&all, &case, run.as_ref());
            println!(
                "| {} | {} | {} | {}/{} | {} / {} | {} / {} |",
                case.query,
                case.group,
                case.inputs.len(),
                ok,
                case.inputs.len(),
                fmt(t1.median_ms),
                fmt(t1.p95_ms),
                fmt(tn.median_ms),
                fmt(tn.p95_ms)
            );
            results.push(json!({"query": case.query, "group": case.group, "starts": case.inputs.len(), "correct": ok,
                "one_thread": {"median_ms": t1.median_ms, "p95_ms": t1.p95_ms}, "all_threads": {"median_ms": tn.median_ms, "p95_ms": tn.p95_ms, "threads": threads}}));
        }
    }

    // ---- whole graph: distance to the nearest banned user, over every link type, backwards, up to 4 steps ----
    let banned = condition::evaluate_packed(
        &graph,
        user,
        &Condition::compare("banned", Op::Eq, Value::Bool(true)),
    );
    let banned_starts: Vec<GlobalId> = banned.ones().map(|l| GlobalId(l as u64)).collect(); // users come first: local id = global id
    let histogram = |pool: &ThreadPool| {
        pool.install(|| {
            let t = Instant::now();
            let r = walk::reach(
                &view,
                &Reach {
                    starts: &banned_starts,
                    link_types: &all_rel,
                    direction: Direction::Backward,
                    max_steps: 4,
                    through: None,
                    cap: None,
                },
            );
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            let mut h = serde_json::Map::new();
            h.insert("0".into(), json!(banned_starts.len()));
            for (d, c) in r.per_step.iter().enumerate() {
                h.insert((d + 1).to_string(), json!(c));
            }
            let reached: u64 = banned_starts.len() as u64 + r.per_step.iter().sum::<u64>();
            h.insert("none".into(), json!(graph.node_count() as u64 - reached));
            (Json::Object(h), ms, r.pull_steps)
        })
    };
    let (h1, ms1, _) = histogram(&one);
    let (hn, msn, pulls) = histogram(&all);
    let ok = h1 == expected["Q7"] && hn == expected["Q7"];
    if !ok {
        wrong_total += 1;
        eprintln!("WRONG Q7: got {hn} expected {}", expected["Q7"]);
    }
    println!("\n## Whole graph\n");
    println!("| Query | Correct | 1 thread | {threads} threads |\n|---|---|---|---|");
    println!(
        "| Q7 distance to nearest banned user, all {} nodes ({pulls} pull steps) | {} | {} ms | {} ms |",
        graph.node_count(),
        if ok { "yes" } else { "no" },
        fmt(ms1),
        fmt(msn)
    );
    results
        .push(json!({"query": "Q7", "correct": ok, "one_thread_ms": ms1, "all_threads_ms": msn}));

    println!("\n{} wrong answers", wrong_total);
    if let Some(path) = out_path {
        let memory: Vec<Json> = graph
            .memory()
            .into_iter()
            .map(|(n, b)| json!({"structure": n, "bytes": b}))
            .collect();
        let doc = json!({"load_seconds": load_s, "peak_rss_bytes": peak, "memory": memory, "results": results, "threads": threads});
        std::fs::write(path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
    }
}
