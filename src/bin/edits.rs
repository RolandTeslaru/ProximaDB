//! Reads while writes arrive: reader threads run path-filter walks while one writer applies edits, with rebuilds
//! starting whenever the edits pass 5% of the packed data.
//!
//!     cargo run --release --bin edits -- <dataset dir> [seconds per phase]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use proximadb::core::Direction;
use proximadb::core::node::GlobalId;
use proximadb::core::{link_type, node_type};
use proximadb::query::condition::{self, Condition, Op, Value};
use proximadb::query::walk::{self, Reach, Through};
use proximadb::{Store, View, load};
use serde_json::Value as Json;

#[path = "shared/dataset.rs"]
mod dataset;
use dataset::{peak_rss_bytes, schema};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn stats(mut v: Vec<f64>) -> String {
    if v.is_empty() {
        return "–".into();
    }
    v.sort_by(f64::total_cmp);
    let p = |q: f64| v[((v.len() as f64 - 1.0) * q).round() as usize];
    format!(
        "{:.2} / {:.2} / {:.2} / {:.1}",
        p(0.5),
        p(0.95),
        p(0.99),
        v[v.len() - 1]
    )
}

enum Writes {
    None,
    Rate(u64),
    Max,
}

/// Building the condition and the walk, one thread, averaged over 20 starts.
fn split(
    view: &View,
    user: node_type::Id,
    follows: link_type::Id,
    active: &Condition,
    starts: &[u64],
) -> (f64, f64) {
    let t = Instant::now();
    let mut through = Through::new(view.graph, false);
    for _ in 0..5 {
        through.set(user, condition::evaluate(view, user, active));
    }
    let eval = t.elapsed().as_secs_f64() * 1000.0 / 5.0;
    let t = Instant::now();
    for &s in starts.iter().take(20) {
        std::hint::black_box(
            walk::reach(
                view,
                &Reach {
                    starts: &[s],
                    link_types: &[follows],
                    direction: Direction::Forward,
                    max_steps: 4,
                    through: Some(&through),
                    cap: None,
                },
            )
            .reached
            .len(),
        );
    }
    (eval, t.elapsed().as_secs_f64() * 1000.0 / 20.0)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = PathBuf::from(args.get(1).map(String::as_str).unwrap_or("research/data/M"));
    let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(8);
    let graph = load::load(&dir, &schema()).expect("load");
    let params: Json =
        serde_json::from_str(&std::fs::read_to_string(dir.join("params.json")).unwrap()).unwrap();
    let user = graph.node_type("User").unwrap();
    let follows = graph.link_type("FOLLOWS").unwrap();
    let users = graph.nodes.count(user); // users come first: their numbers are 0..users, before and after rebuilds
    let packed_links: usize = graph.link_types.iter().map(|r| r.forward.len()).sum();
    let packed_nodes = graph.node_count();
    // Ordinary (non-hub) start users, by key, so they can be found again in any generation.
    let start_keys: Vec<Vec<u8>> = params["followers"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| !r["hub"].as_bool().unwrap())
        .map(|r| r["nodeId"].as_u64().unwrap().to_string().into_bytes())
        .collect();
    let starts_in = |view: &View| -> Vec<u64> {
        start_keys
            .iter()
            .map(|k| view.lookup(user, k).unwrap())
            .collect()
    };
    let active = Condition::compare("status", Op::Eq, Value::Str("active".into()));
    let store = Store::new(graph);
    let readers = 8;
    let seq = AtomicU64::new(0);

    let clean = {
        let g = store.current();
        let v = g.view();
        split(&v, user, follows, &active, &starts_in(&v))
    };

    println!(
        "# Reads while writes arrive, with rebuilds, {packed_nodes} nodes, {packed_links} connections\n"
    );
    println!(
        "{readers} reader threads, each running \"within 4 follows, only through active users\" from ordinary starts, \
         building the condition every query, on the current generation. One writer thread. {secs} s per phase.\n"
    );
    println!(
        "Each write is one action: 70% add a follow, 20% change a user's status, 10% remove a follow added earlier.\n"
    );
    println!(
        "A rebuild starts when links added or removed pass 5% of the packed connections ({}), or node edits (status \
         changes here) pass 5% of the packed nodes ({}).\n",
        packed_links / 20,
        packed_nodes / 20
    );
    println!(
        "| Writes | Reader ms (median / p95 / p99 / max) | Reader queries/s | Actions written/s | Write ms per action (median / p95 / p99 / max) | Rebuilds finished |\n|---|---|---|---|---|---|"
    );

    let mut writer_rng = Rng(0x2545_F491_4F6C_DD1D);
    let mut added: Vec<(u64, u64)> = Vec::new();
    let mut seen_stats = 0;
    let mut all_stats = Vec::new();
    for (label, mode) in [
        ("none", Writes::None),
        ("1,000 actions/s", Writes::Rate(1_000)),
        ("10,000 actions/s", Writes::Rate(10_000)),
        ("as fast as possible", Writes::Max),
        ("none (after the writes above)", Writes::None),
    ] {
        let stop = AtomicBool::new(false);
        let before = store.rebuilds();
        let (reads, writes, write_count, stats_now) = std::thread::scope(|scope| {
            let reader_handles: Vec<_> = (0..readers)
                .map(|r| {
                    let (store, active, stop, starts_in) = (&store, &active, &stop, &starts_in);
                    scope.spawn(move || {
                        let pool = rayon::ThreadPoolBuilder::new()
                            .num_threads(1)
                            .build()
                            .unwrap();
                        pool.install(|| {
                            let mut times = Vec::new();
                            let mut i = r;
                            let mut starts = (0usize, Vec::new()); // (generation address, start numbers in it)
                            while !stop.load(Ordering::Relaxed) {
                                let t = Instant::now();
                                let generation = store.current();
                                let view = generation.view();
                                let id = std::sync::Arc::as_ptr(&generation) as usize;
                                if starts.0 != id {
                                    starts = (id, starts_in(&view)); // only after a swap
                                }
                                let mut through = Through::new(view.graph, false);
                                through.set(user, condition::evaluate(&view, user, active));
                                let s = starts.1[i % starts.1.len()];
                                let res = walk::reach(
                                    &view,
                                    &Reach {
                                        starts: &[s],
                                        link_types: &[follows],
                                        direction: Direction::Forward,
                                        max_steps: 4,
                                        through: Some(&through),
                                        cap: None,
                                    },
                                );
                                std::hint::black_box(res.reached.len());
                                times.push(t.elapsed().as_secs_f64() * 1000.0);
                                i += readers;
                            }
                            times
                        })
                    })
                })
                .collect();
            let writer = {
                let (store, stop, seq, added, rng) =
                    (&store, &stop, &seq, &mut added, &mut writer_rng);
                scope.spawn(move || {
                    let mut times = Vec::new();
                    let mut w = store.writer();
                    if matches!(mode, Writes::None) {
                        while !stop.load(Ordering::Relaxed) {
                            w.idle();
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        return (times, 0u64, w.rebuild_stats().to_vec());
                    }
                    let started = Instant::now();
                    let mut done = 0u64;
                    while !stop.load(Ordering::Relaxed) {
                        if let Writes::Rate(rate) = mode {
                            let due = started + Duration::from_secs_f64(done as f64 / rate as f64);
                            let now = Instant::now();
                            if due > now {
                                std::thread::sleep(due - now);
                            }
                        }
                        let t = Instant::now();
                        let s = seq.fetch_add(1, Ordering::Relaxed) + 1;
                        w.begin(s);
                        let roll = rng.next() % 10;
                        if roll < 7 || (roll == 9 && added.is_empty()) {
                            let (a, b) = (rng.next() % users, rng.next() % users);
                            w.add_link(follows, GlobalId(a), GlobalId(b));
                            added.push((a, b));
                        } else if roll < 9 {
                            let status = ["active", "inactive"][(rng.next() % 2) as usize];
                            w.set_property(
                                GlobalId(rng.next() % users),
                                "status",
                                &Value::Str(status.into()),
                            );
                        } else {
                            let i = (rng.next() % added.len() as u64) as usize;
                            let (a, b) = added.swap_remove(i);
                            w.remove_link(follows, GlobalId(a), GlobalId(b));
                        }
                        w.commit();
                        times.push(t.elapsed().as_secs_f64() * 1000.0);
                        done += 1;
                    }
                    (times, done, w.rebuild_stats().to_vec())
                })
            };
            std::thread::sleep(Duration::from_secs(secs));
            stop.store(true, Ordering::Relaxed);
            let reads: Vec<f64> = reader_handles
                .into_iter()
                .flat_map(|h| h.join().unwrap())
                .collect();
            let (writes, count, st) = writer.join().unwrap();
            (reads, writes, count, st)
        });
        println!(
            "| {label} | {} | {:.0} | {:.0} | {} | {} |",
            stats(reads.clone()),
            reads.len() as f64 / secs as f64,
            write_count as f64 / secs as f64,
            stats(writes),
            store.rebuilds() - before
        );
        all_stats.extend_from_slice(&stats_now[seen_stats..]);
        seen_stats = stats_now.len();
    }

    if !all_stats.is_empty() {
        println!("\n## The rebuilds\n");
        println!(
            "| # | Build (own thread) ms | Replayed in the background: actions / rounds / ms | Left for the writer: actions | Writer paused ms |\n|---|---|---|---|---|"
        );
        for (i, s) in all_stats.iter().enumerate() {
            println!(
                "| {} | {:.0} | {} / {} / {:.0} | {} | {:.2} |",
                i + 1,
                s.build.as_secs_f64() * 1000.0,
                s.replayed_behind,
                s.rounds,
                s.catch_up.as_secs_f64() * 1000.0,
                s.replayed_paused,
                s.pause.as_secs_f64() * 1000.0
            );
        }
    }

    let g = store.current();
    let v = g.view();
    println!(
        "\nCurrent generation: {} nodes, {} connections packed; memory held by its edits {:.1} MB; peak memory of the process {:.2} GB.",
        v.node_count(),
        g.graph
            .link_types
            .iter()
            .map(|r| r.forward.len())
            .sum::<usize>(),
        g.live.bytes() as f64 / 1e6,
        peak_rss_bytes() as f64 / 1e9
    );
    let now = split(&v, user, follows, &active, &starts_in(&v));
    drop(v);
    drop(g);
    let mut w = store.writer();
    w.rebuild_and_wait();
    drop(w);
    let g = store.current();
    let v = g.view();
    let packed = split(&v, user, follows, &active, &starts_in(&v));
    println!("\n## One thread: building the condition and the walk\n");
    println!("| State | Building the condition ms | The walk ms |\n|---|---|---|");
    println!("| before any edits | {:.1} | {:.1} |", clean.0, clean.1);
    println!(
        "| after the phases (edits since the last rebuild on top) | {:.1} | {:.1} |",
        now.0, now.1
    );
    println!(
        "| after one more rebuild (everything packed) | {:.1} | {:.1} |",
        packed.0, packed.1
    );
}
