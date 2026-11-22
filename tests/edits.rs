//! Random edits applied through the writer must give the same answers as the final state rebuilt from scratch.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use proximadb::Graph;
use proximadb::core::Direction;
use proximadb::core::Kind;
use proximadb::core::node::GlobalId;
use proximadb::load::{self, Backing, LinkTypeSpec, NodeTypeSpec, Schema};
use proximadb::query::condition::{self, Condition, Op, Value};
use proximadb::query::walk::{self, Reach, Through};
use proximadb::{Live, Store, View};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn schema() -> Schema {
    Schema {
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
                properties: vec![("color".into(), Kind::String)],
            },
        ],
        link_types: vec![
            LinkTypeSpec {
                name: "FOLLOWS".into(),
                from_type: "Person".into(),
                to_type: "Person".into(),
                backing: Backing::Dataset {
                    file: "FOLLOWS.csv".into(),
                    from_column: "src".into(),
                    to_column: "dst".into(),
                },
            },
            LinkTypeSpec {
                name: "OWNS".into(),
                from_type: "Person".into(),
                to_type: "Thing".into(),
                backing: Backing::Dataset {
                    file: "OWNS.csv".into(),
                    from_column: "src".into(),
                    to_column: "dst".into(),
                },
            },
        ],
    }
}

/// The intended final state, kept by the test.
#[derive(Clone, Default)]
struct Model {
    persons: BTreeMap<String, (String, i64)>, // key → (status, age)
    things: BTreeMap<String, String>,
    follows: Vec<(String, String)>,
    owns: Vec<(String, String)>,
}

impl Model {
    fn write(&self, dir: &PathBuf) {
        fs::create_dir_all(dir).unwrap();
        let mut p = String::from("key,status,age\n");
        for (k, (s, a)) in &self.persons {
            p += &format!("{k},{s},{a}\n");
        }
        fs::write(dir.join("Person.csv"), p).unwrap();
        let mut t = String::from("key,color\n");
        for (k, c) in &self.things {
            t += &format!("{k},{c}\n");
        }
        fs::write(dir.join("Thing.csv"), t).unwrap();
        for (name, list) in [("FOLLOWS", &self.follows), ("OWNS", &self.owns)] {
            let mut e = String::from("src,dst\n");
            for (a, b) in list {
                e += &format!("{a},{b}\n");
            }
            fs::write(dir.join(format!("{name}.csv")), e).unwrap();
        }
    }
}

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("proximadb-test-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    d
}

fn random_model(
    rng: &mut Rng,
    persons: usize,
    things: usize,
    follows: usize,
    owns: usize,
) -> Model {
    let mut m = Model::default();
    for i in 0..persons {
        m.persons.insert(
            format!("p{i}"),
            (
                ["active", "inactive", "banned"][rng.below(3)].to_string(),
                18 + rng.below(60) as i64,
            ),
        );
    }
    for i in 0..things {
        m.things
            .insert(format!("t{i}"), ["red", "blue"][rng.below(2)].to_string());
    }
    let pk: Vec<String> = m.persons.keys().cloned().collect();
    let tk: Vec<String> = m.things.keys().cloned().collect();
    for _ in 0..follows {
        m.follows.push((
            pk[rng.below(pk.len())].clone(),
            pk[rng.below(pk.len())].clone(),
        ));
    }
    for _ in 0..owns {
        m.owns.push((
            pk[rng.below(pk.len())].clone(),
            tk[rng.below(tk.len())].clone(),
        ));
    }
    m
}

fn node(view: &View, t: &str, key: &str) -> Option<GlobalId> {
    view.lookup(view.graph.node_type(t).unwrap(), key.as_bytes())
}

/// Everything compared between the edited view and the rebuilt graph, for one start node, by keys.
fn answers(view: &View, start: GlobalId) -> Vec<BTreeSet<Vec<u8>>> {
    let g = view.graph;
    let all = g.all_link_types();
    let person = g.node_type("Person").unwrap();
    let active = Condition::compare("status", Op::Eq, Value::Str("active".into()));
    let mut through = Through::new(g, true);
    through.set(person, condition::evaluate(view, person, &active));
    let mut out = Vec::new();
    for (direction, through) in [
        (Direction::Forward, None),
        (Direction::Backward, None),
        (Direction::Both, None),
        (Direction::Forward, Some(&through)),
    ] {
        let r = walk::reach(
            view,
            &Reach {
                starts: &[start],
                link_types: &all,
                direction,
                max_steps: 4,
                through,
                cap: None,
            },
        );
        out.push(r.reached.iter().map(|&v| view.key(v)).collect());
    }
    // People over 40 as of this view.
    let older = condition::evaluate(
        view,
        person,
        &Condition::compare("age", Op::Gt, Value::Num(40.0)),
    );
    let mut over = BTreeSet::new();
    for local in older.ones() {
        // find the node with this local id
        for g in (0..view.nodes).map(GlobalId) {
            let (t, l) = view.get(g);
            if t == person && l.0 as usize == local && !view.is_deleted(g) {
                over.insert(view.key(g));
            }
        }
    }
    out.push(over);
    out
}

fn compare(view: &View, fresh: &Graph, model: &Model, rng: &mut Rng, label: &str) {
    let fresh_live = Live::new(fresh);
    let fv = fresh_live.view(fresh);
    let keys: Vec<&String> = model.persons.keys().collect();
    for _ in 0..40 {
        let k = keys[rng.below(keys.len())];
        let a = node(view, "Person", k).expect("edited view has the person");
        let b = node(&fv, "Person", k).expect("rebuilt graph has the person");
        assert_eq!(
            answers(view, a),
            answers(&fv, b),
            "{label}: answers differ from {k}"
        );
        let k2 = keys[rng.below(keys.len())];
        let a2 = node(view, "Person", k2).unwrap();
        let b2 = node(&fv, "Person", k2).unwrap();
        let all = view.graph.all_link_types();
        assert_eq!(
            walk::path_length(view, a, a2, &all, Direction::Forward, 8, None),
            walk::path_length(
                &fv,
                b,
                b2,
                &fresh.all_link_types(),
                Direction::Forward,
                8,
                None
            ),
            "{label}: path {k} → {k2}"
        );
    }
}

#[test]
fn random_edits_match_a_rebuild() {
    for round in 0..6 {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ ((round as u64 + 1) * 0x1234_5678));
        let mut model = random_model(&mut rng, 120, 30, 360, 80);
        let dir = temp(&format!("base-{round}"));
        model.write(&dir);
        let graph = load::load(&dir, &schema()).unwrap();
        let person = graph.node_type("Person").unwrap();
        let thing = graph.node_type("Thing").unwrap();
        let follows = graph.link_type("FOLLOWS").unwrap();
        let owns = graph.link_type("OWNS").unwrap();
        // Rebuild at 5% of the packed data with no minimum: several rebuilds per round, with actions replayed.
        let store = Store::with_limits(graph, 0.05, 0).with_catch_up(if round % 2 == 0 {
            1
        } else {
            1_000
        });
        let mut deleted: BTreeSet<String> = BTreeSet::new();
        let mut w = store.writer();
        {
            let mut next = 1000;
            for seq in 1..=400u64 {
                w.begin(seq);
                let generation = w.generation().clone();
                let view = generation.view();
                let alive: Vec<String> = model
                    .persons
                    .keys()
                    .filter(|k| !deleted.contains(*k))
                    .cloned()
                    .collect();
                let pick = |rng: &mut Rng| alive[rng.below(alive.len())].clone();
                match rng.below(10) {
                    0 => {
                        let key = format!("p{next}");
                        next += 1;
                        let g = w.add_node(person, key.as_bytes());
                        let status = ["active", "inactive", "suspended"][rng.below(3)].to_string();
                        let age = 18 + rng.below(60) as i64;
                        w.set_property(g, "status", &Value::Str(status.clone()));
                        w.set_property(g, "age", &Value::Num(age as f64));
                        model.persons.insert(key, (status, age));
                    }
                    1 => {
                        let key = format!("t{next}");
                        next += 1;
                        let g = w.add_node(thing, key.as_bytes());
                        w.set_property(g, "color", &Value::Str("green".into()));
                        model.things.insert(key, "green".into());
                    }
                    2..=4 => {
                        let (a, b) = (pick(&mut rng), pick(&mut rng));
                        w.add_link(
                            follows,
                            node(&view, "Person", &a).unwrap(),
                            node(&view, "Person", &b).unwrap(),
                        );
                        model.follows.push((a, b));
                    }
                    5 => {
                        let things: Vec<String> = model.things.keys().cloned().collect();
                        let (a, b) = (pick(&mut rng), things[rng.below(things.len())].clone());
                        w.add_link(
                            owns,
                            node(&view, "Person", &a).unwrap(),
                            node(&view, "Thing", &b).unwrap(),
                        );
                        model.owns.push((a, b));
                    }
                    6 | 7 => {
                        if !model.follows.is_empty() {
                            let i = rng.below(model.follows.len());
                            let (a, b) = model.follows[i].clone();
                            if !deleted.contains(&a) && !deleted.contains(&b) {
                                assert!(w.remove_link(
                                    follows,
                                    node(&view, "Person", &a).unwrap(),
                                    node(&view, "Person", &b).unwrap()
                                ));
                                model.follows.remove(i);
                            }
                        }
                    }
                    8 => {
                        let a = pick(&mut rng);
                        let status = ["active", "inactive"][rng.below(2)].to_string();
                        let age = 18 + rng.below(60) as i64;
                        let g = node(&view, "Person", &a).unwrap();
                        w.set_property(g, "status", &Value::Str(status.clone()));
                        w.set_property(g, "age", &Value::Num(age as f64));
                        model.persons.insert(a, (status, age));
                    }
                    _ => {
                        if alive.len() > 40 {
                            let a = pick(&mut rng);
                            w.delete_node(node(&view, "Person", &a).unwrap());
                            deleted.insert(a);
                        }
                    }
                }
                w.commit();

                // A rebuild counts once swapped in, at a commit after it finished: on a slow machine none may
                // have by the end, so wait for the running one now and then.
                if seq % 50 == 0 {
                    w.finish_rebuild();
                }
            }
        }
        // The rebuilt graph: deleted persons and their links gone.
        let mut fresh_model = model.clone();
        fresh_model.persons.retain(|k, _| !deleted.contains(k));
        fresh_model
            .follows
            .retain(|(a, b)| !deleted.contains(a) && !deleted.contains(b));
        fresh_model.owns.retain(|(a, _)| !deleted.contains(a));
        let fresh_dir = temp(&format!("fresh-{round}"));
        fresh_model.write(&fresh_dir);
        let fresh = load::load(&fresh_dir, &schema()).unwrap();
        assert!(
            store.rebuilds() >= 3,
            "round {round}: only {} rebuilds",
            store.rebuilds()
        );
        // As the writer left it: a generation with edits on top, maybe a rebuild still running.
        let generation = store.current();
        compare(
            &generation.view(),
            &fresh,
            &fresh_model,
            &mut rng,
            &format!("round {round}"),
        );
        // Everything folded into the packed graph.
        w.rebuild_and_wait();
        let generation = store.current();
        assert_eq!(generation.live.side_chunks(), 0);
        compare(
            &generation.view(),
            &fresh,
            &fresh_model,
            &mut rng,
            &format!("round {round}, after a full rebuild"),
        );
        drop(w);
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&fresh_dir);
    }
}

#[test]
fn a_search_never_sees_half_an_action() {
    let mut rng = Rng(42);
    let model = random_model(&mut rng, 20, 2, 0, 0);
    let dir = temp("snapshot");
    model.write(&dir);
    let graph = load::load(&dir, &schema()).unwrap();
    let follows = graph.link_type("FOLLOWS").unwrap();
    let all = graph.all_link_types();
    let person = graph.node_type("Person").unwrap();
    let store = Store::new(graph);
    let g0 = store.current();
    let before = g0.view();
    let a = node(&before, "Person", "p0").unwrap();
    let b = node(&before, "Person", "p1").unwrap();
    let mut w = store.writer();
    w.begin(1);
    w.add_link(follows, a, b);
    // Written but not published: no view sees it, forward or backward.
    let g1 = store.current();
    let during = g1.view();
    assert_eq!(
        walk::path_length(&during, a, b, &all, Direction::Forward, 3, None),
        None
    );
    assert_eq!(
        walk::path_length(&during, b, a, &all, Direction::Backward, 3, None),
        None
    );
    w.commit();
    // A view taken before the commit still doesn't see it; a new one does.
    assert_eq!(
        walk::path_length(&before, a, b, &all, Direction::Forward, 3, None),
        None
    );
    let g2 = store.current();
    let after = g2.view();
    assert_eq!(
        walk::path_length(&after, a, b, &all, Direction::Forward, 3, None),
        Some(1)
    );
    assert_eq!(
        walk::path_length(&after, b, a, &all, Direction::Backward, 3, None),
        Some(1)
    );
    // A property change is only visible to views after its commit.
    let cond = Condition::compare("status", Op::Eq, Value::Str("vip".into()));
    w.begin(2);
    w.set_property(a, "status", &Value::Str("vip".into()));
    assert_eq!(
        condition::evaluate(&store.current().view(), person, &cond).count(),
        0
    );
    w.commit();
    assert_eq!(
        condition::evaluate(&store.current().view(), person, &cond).count(),
        1
    );
    assert_eq!(condition::evaluate(&after, person, &cond).count(), 0);
    let _ = condition::evaluate_packed; // the packed columns are untouched
    // A rebuild swaps in a new generation; a search holding the old one keeps its answers.
    w.rebuild_and_wait();
    assert_eq!(store.rebuilds(), 1);
    assert_eq!(
        walk::path_length(&before, a, b, &all, Direction::Forward, 3, None),
        None
    );
    assert_eq!(condition::evaluate(&after, person, &cond).count(), 0);
    let g3 = store.current();
    let now = g3.view();
    assert_eq!(
        now.live.side_chunks(),
        0,
        "the new generation has the link packed"
    );
    let (a3, b3) = (
        node(&now, "Person", "p0").unwrap(),
        node(&now, "Person", "p1").unwrap(),
    );
    assert_eq!(
        walk::path_length(&now, a3, b3, &all, Direction::Forward, 3, None),
        Some(1)
    );
    assert_eq!(condition::evaluate(&now, person, &cond).count(), 1);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn an_idle_writer_still_rebuilds() {
    let mut rng = Rng(7);
    let model = random_model(&mut rng, 50, 2, 100, 0);
    let dir = temp("idle");
    model.write(&dir);
    let graph = load::load(&dir, &schema()).unwrap();
    let follows = graph.link_type("FOLLOWS").unwrap();
    let all = graph.all_link_types();
    let store = Store::with_limits(graph, 0.05, 0); // 5% of 100 links: the 6th link edit starts a rebuild
    let mut w = store.writer();
    let (a, b) = {
        let g = store.current();
        let v = g.view();
        (
            node(&v, "Person", "p0").unwrap(),
            node(&v, "Person", "p1").unwrap(),
        )
    };
    for seq in 1..=6 {
        w.begin(seq);
        w.add_link(follows, a, b);
        w.commit();
    }
    assert!(w.is_rebuilding(), "the last action started a rebuild");
    assert_eq!(store.rebuilds(), 0);
    // No more actions arrive: only idle checks can swap it in.
    let started = std::time::Instant::now();
    while w.is_rebuilding() {
        assert!(started.elapsed().as_secs() < 10, "rebuild never swapped in");
        w.idle();
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert_eq!(store.rebuilds(), 1);
    let g = store.current();
    let v = g.view();
    assert_eq!(v.live.side_chunks(), 0, "all six links are packed");
    let (a2, b2) = (
        node(&v, "Person", "p0").unwrap(),
        node(&v, "Person", "p1").unwrap(),
    );
    assert_eq!(
        walk::path_length(&v, a2, b2, &all, Direction::Forward, 3, None),
        Some(1)
    );
    let _ = fs::remove_dir_all(&dir);
}
