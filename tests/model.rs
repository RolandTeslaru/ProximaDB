//! ProximaDB against a model too simple to be wrong: random graphs, random edits and rebuilds, and after each step
//! random questions whose answers must match the model's, found with plain loops over plain lists.
//!
//! A failure names its seed. `PROXIMADB_SEEDS=500` runs more of them.

mod common;

use std::collections::{BTreeSet, HashMap};

use common::{Dir, Rng, link_type, node_type};
use proximadb::core::node::GlobalId;
use proximadb::core::{Direction, Kind, Value};
use proximadb::load::{self, Schema};
use proximadb::query::condition::{self, Condition, Op};
use proximadb::query::cypher;
use proximadb::query::subgraph::{self, Role};
use proximadb::query::walk::{self, Reach, Through};
use proximadb::storage::link_type::Id as LinkTypeId;
use proximadb::storage::node_type::Id as TypeId;
use proximadb::{Store, View};

const SEEDS: u64 = 48;
const QUESTIONS: usize = 25;
const TYPES: [&str; 3] = ["Person", "Thing", "Place"];

/// Link type, from its type, to its type.
const LINKS: [(&str, usize, usize); 3] = [("FOLLOWS", 0, 0), ("OWNS", 0, 1), ("AT", 1, 2)];

/// A person's properties: one of every kind.
const PROPERTIES: [(&str, Kind); 7] = [
    ("status", Kind::String),
    ("age", Kind::Integer),
    ("big", Kind::Long),
    ("spent", Kind::Double),
    ("banned", Kind::Boolean),
    ("joined", Kind::Date),
    ("seen", Kind::Timestamp),
];


// ---------------------------------------------------------------- the model


#[derive(Clone, Debug)]
enum Cell {
    Str(String),
    Num(f64),
    Bool(bool),
}


struct Node {
    node_type: usize,
    key: String,
    values: Vec<Option<Cell>>,
    alive: bool,
}


struct Model {
    nodes: Vec<Node>,
    /// (link type, from, to); a pair may repeat.
    links: Vec<(usize, usize, usize)>,
}


#[derive(Debug)]
enum Test {
    Compare(usize, Op, Cell),
    Has(usize),
    And(Vec<Test>),
    Or(Vec<Test>),
    Not(Box<Test>),
}


impl Cell {
    fn random(kind: Kind, rng: &mut Rng) -> Self {
        match kind {
            Kind::String => {
                Cell::Str(["active", "inactive", "banned", "Ș", "two words"][rng.below(5)].into())
            }
            Kind::Integer => Cell::Num([-5.0, 0.0, 1.0, 30.0, 41.0, 2_147_483_647.0][rng.below(6)]),
            Kind::Long => Cell::Num([-3_000_000_000.0, 0.0, 7.0, 4_000_000_000.0][rng.below(4)]),
            Kind::Double => Cell::Num([-2.5, -0.0, 0.0, 0.1, 100.0, 499.99, 1e12][rng.below(7)]),
            Kind::Boolean => Cell::Bool(rng.chance(50)),
            Kind::Date => Cell::Num([-1.0, 0.0, 19_782.0, 20_000.0][rng.below(4)]),
            Kind::Timestamp => Cell::Num([0.0, 1_000_000.0, 1_767_254_400_000_000.0][rng.below(3)]),
        }
    }


    fn value(&self) -> Value {
        match self {
            Cell::Str(text) => Value::Str(text.clone()),
            Cell::Num(number) => Value::Num(*number),
            Cell::Bool(flag) => Value::Bool(*flag),
        }
    }


    fn csv(&self, kind: Kind) -> String {
        match (self, kind) {
            (Cell::Str(text), _) => text.clone(),
            (Cell::Bool(flag), _) => flag.to_string(),
            (Cell::Num(days), Kind::Date) => match *days as i64 {
                -1 => "1969-12-31".into(),
                0 => "1970-01-01".into(),
                19_782 => "2024-02-29".into(),
                _ => "2024-10-04".into(),
            },
            (Cell::Num(micros), Kind::Timestamp) => match *micros as i64 {
                0 => "1970-01-01 00:00:00".into(),
                1_000_000 => "1970-01-01 00:00:01".into(),
                _ => "2026-01-01T08:00:00Z".into(),
            },
            (Cell::Num(number), Kind::Double) => number.to_string(),
            (Cell::Num(number), _) => (*number as i64).to_string(),
        }
    }
}


impl Test {
    fn random(rng: &mut Rng, depth: usize) -> Self {
        let parts = |rng: &mut Rng| {
            (0..1 + rng.below(3))
                .map(|_| Test::random(rng, depth - 1))
                .collect()
        };

        if depth > 0 && rng.chance(45) {
            return match rng.below(3) {
                0 => Test::And(parts(rng)),
                1 => Test::Or(parts(rng)),
                _ => Test::Not(Box::new(Test::random(rng, depth - 1))),
            };
        }

        let property = rng.below(PROPERTIES.len());

        if rng.chance(15) {
            return Test::Has(property);
        }

        let kind = PROPERTIES[property].1;

        // Strings and booleans by equality only: whether strings can be ordered depends on the data.
        let ops: &[Op] = match kind {
            Kind::String | Kind::Boolean => &[Op::Eq, Op::Ne],
            _ => &[Op::Eq, Op::Ne, Op::Lt, Op::Le, Op::Gt, Op::Ge],
        };

        Test::Compare(property, ops[rng.below(ops.len())], Cell::random(kind, rng))
    }


    fn condition(&self) -> Condition {
        match self {
            Test::Compare(property, op, cell) => {
                Condition::compare(PROPERTIES[*property].0, *op, cell.value())
            }
            Test::Has(property) => Condition::HasValue(PROPERTIES[*property].0.into()),
            Test::And(parts) => Condition::And(parts.iter().map(Test::condition).collect()),
            Test::Or(parts) => Condition::Or(parts.iter().map(Test::condition).collect()),
            Test::Not(inner) => Condition::Not(Box::new(inner.condition())),
        }
    }


    fn holds(&self, values: &[Option<Cell>]) -> bool {
        let (property, op, wanted) = match self {
            Test::Has(property) => return values[*property].is_some(),
            Test::And(parts) => return parts.iter().all(|part| part.holds(values)),
            Test::Or(parts) => return parts.iter().any(|part| part.holds(values)),
            Test::Not(inner) => return !inner.holds(values),
            Test::Compare(property, op, wanted) => (*property, *op, wanted),
        };

        let order = match (&values[property], wanted) {
            (Some(Cell::Str(a)), Cell::Str(b)) => a.cmp(b),
            (Some(Cell::Num(a)), Cell::Num(b)) => a.partial_cmp(b).unwrap(),
            (Some(Cell::Bool(a)), Cell::Bool(b)) => a.cmp(b),
            _ => return false,
        };

        match op {
            Op::Eq => order.is_eq(),
            Op::Ne => order.is_ne(),
            Op::Lt => order.is_lt(),
            Op::Le => order.is_le(),
            Op::Gt => order.is_gt(),
            Op::Ge => order.is_ge(),
        }
    }
}


impl Model {
    fn random(rng: &mut Rng, size: usize) -> Self {
        let mut model = Model {
            nodes: vec![],
            links: vec![],
        };

        for (node_type, count) in [size, size / 3, size / 8].into_iter().enumerate() {
            for i in 0..count {
                model.add(node_type, format!("{}{i}", TYPES[node_type]), rng);
            }
        }

        for _ in 0..size * 3 {
            if let Some(link) = model.random_link(rng) {
                model.links.push(link);
            }
        }

        model
    }


    fn add(&mut self, node_type: usize, key: String, rng: &mut Rng) -> usize {
        let values = match node_type {
            0 => PROPERTIES
                .iter()
                .map(|&(_, kind)| (!rng.chance(20)).then(|| Cell::random(kind, rng)))
                .collect(),
            _ => vec![],
        };

        self.nodes.push(Node {
            node_type,
            key,
            values,
            alive: true,
        });

        self.nodes.len() - 1
    }


    fn alive(&self, node_type: usize) -> Vec<usize> {
        (0..self.nodes.len())
            .filter(|&n| self.nodes[n].alive && self.nodes[n].node_type == node_type)
            .collect()
    }


    fn random_link(&self, rng: &mut Rng) -> Option<(usize, usize, usize)> {
        let link = rng.below(LINKS.len());
        let froms = self.alive(LINKS[link].1);
        let tos = self.alive(LINKS[link].2);

        if froms.is_empty() || tos.is_empty() {
            return None;
        }

        // A third of the links start at one of three hubs, so that walks switch to pulling.
        let from = if rng.chance(33) {
            froms[rng.below(froms.len().min(3))]
        } else {
            froms[rng.below(froms.len())]
        };

        Some((link, from, tos[rng.below(tos.len())]))
    }


    fn write(&self, dir: &Dir) -> Schema {
        let mut files: Vec<String> = vec!["key".into(), "key\n".into(), "key\n".into()];

        for (name, _) in PROPERTIES {
            files[0] += &format!(",{name}");
        }

        files[0] += "\n";

        for node in &self.nodes {
            let mut row = node.key.clone();

            for (value, (_, kind)) in node.values.iter().zip(PROPERTIES) {
                row += ",";
                row += &value.as_ref().map_or(String::new(), |cell| cell.csv(kind));
            }

            files[node.node_type] += &row;
            files[node.node_type] += "\n";
        }

        for (name, text) in TYPES.iter().zip(&files) {
            dir.write(&format!("{name}.csv"), text);
        }

        for (i, (name, _, _)) in LINKS.iter().enumerate() {
            let mut text = String::from("a,b\n");

            for &(link, from, to) in &self.links {
                if link == i {
                    text += &format!("{},{}\n", self.nodes[from].key, self.nodes[to].key);
                }
            }

            dir.write(&format!("{name}.csv"), text);
        }

        Schema {
            node_types: vec![
                node_type("Person", "Person.csv", &PROPERTIES),
                node_type("Thing", "Thing.csv", &[]),
                node_type("Place", "Place.csv", &[]),
            ],
            link_types: LINKS
                .iter()
                .map(|&(name, from, to)| {
                    link_type(name, TYPES[from], TYPES[to], &format!("{name}.csv"))
                })
                .collect(),
        }
    }


    fn neighbours(&self, chosen: &[usize], direction: Direction) -> Vec<Vec<usize>> {
        let mut out = vec![Vec::new(); self.nodes.len()];

        for &(link, from, to) in &self.links {
            if !chosen.contains(&link) {
                continue;
            }

            if direction != Direction::Backward {
                out[from].push(to);
            }

            if direction != Direction::Forward {
                out[to].push(from);
            }
        }

        out
    }
}


/// Every node's shortest distance in steps from a start, at most `max`. With `marked` the starts are at distance 0;
/// without, a start has the distance at which some start reaches it.
fn distances(
    neighbours: &[Vec<usize>],
    starts: &[usize],
    max: u32,
    marked: bool,
    allowed: &dyn Fn(usize) -> bool,
) -> Vec<Option<u32>> {
    let mut distance = vec![None; neighbours.len()];

    if marked {
        starts.iter().for_each(|&start| distance[start] = Some(0));
    }

    let mut step = starts.to_vec();

    for depth in 1..=max {
        let mut next = Vec::new();

        for &u in &step {
            for &v in &neighbours[u] {
                if distance[v].is_none() && allowed(v) {
                    distance[v] = Some(depth);
                    next.push(v);
                }
            }
        }

        step = next;
    }

    distance
}


// ---------------------------------------------------------------- one seed


struct Run {
    seed: u64,
    rng: Rng,
    model: Model,
    store: Store,
    seq: u64,
    added: usize,
    pools: [rayon::ThreadPool; 2],
}


impl Run {
    fn new(seed: u64, size: usize) -> Self {
        let mut rng = Rng(seed);
        let model = Model::random(&mut rng, size);
        let dir = Dir::new();
        let schema = model.write(&dir);
        let graph = load::load(dir.path(), &schema).unwrap();

        let store =
            Store::with_limits(graph, 0.05, rng.below(50) as u64).with_catch_up(1 + rng.below(100));
        let pools = [1, 6].map(|n| {
            rayon::ThreadPoolBuilder::new()
                .num_threads(n)
                .build()
                .unwrap()
        });

        Self {
            seed,
            rng,
            model,
            store,
            seq: 0,
            added: 0,
            pools,
        }
    }


    fn edit(&mut self) {
        let mut writer = self.store.writer();

        for _ in 0..1 + self.rng.below(12) {
            self.seq += 1;

            let ids: HashMap<usize, GlobalId> = {
                let view = writer.generation().view();

                (0..self.model.nodes.len())
                    .filter(|&n| self.model.nodes[n].alive)
                    .map(|n| {
                        let node = &self.model.nodes[n];

                        (
                            n,
                            view.lookup(TypeId(node.node_type as u16), node.key.as_bytes())
                                .unwrap(),
                        )
                    })
                    .collect()
            };

            let mut ids = ids;
            let follows: Vec<LinkTypeId> = LINKS
                .iter()
                .map(|(name, _, _)| writer.generation().graph.link_type(name).unwrap())
                .collect();

            writer.begin(self.seq);

            // At least one edit in every action: an empty one during a rebuild is a known bug.
            self.added += 1;
            let key = format!("added{}", self.added);
            let node_type = self.rng.below(3);
            let n = self.model.add(node_type, key.clone(), &mut self.rng);
            let g = writer.add_node(TypeId(node_type as u16), key.as_bytes());

            for (value, (name, _)) in self.model.nodes[n].values.iter().zip(PROPERTIES) {
                if let Some(cell) = value {
                    writer.set_property(g, name, &cell.value());
                }
            }

            ids.insert(n, g);

            for _ in 0..self.rng.below(5) {
                match self.rng.below(10) {
                    0..=3 => {
                        if let Some((link, from, to)) = self.model.random_link(&mut self.rng) {
                            writer.add_link(follows[link], ids[&from], ids[&to]);
                            self.model.links.push((link, from, to));
                        }
                    }

                    4..=5 if !self.model.links.is_empty() => {
                        let at = self.rng.below(self.model.links.len());
                        let (link, from, to) = self.model.links.swap_remove(at);

                        assert!(
                            writer.remove_link(follows[link], ids[&from], ids[&to]),
                            "seed {}: a link that exists wasn't removed",
                            self.seed
                        );
                    }

                    6..=8 => {
                        let people = self.model.alive(0);

                        if people.is_empty() {
                            continue;
                        }

                        let person = people[self.rng.below(people.len())];
                        let property = self.rng.below(PROPERTIES.len());
                        let cell = Cell::random(PROPERTIES[property].1, &mut self.rng);

                        writer.set_property(ids[&person], PROPERTIES[property].0, &cell.value());
                        self.model.nodes[person].values[property] = Some(cell);
                    }

                    _ => {
                        let dead = self.rng.below(self.model.nodes.len());

                        if !self.model.nodes[dead].alive {
                            continue;
                        }

                        writer.delete_node(ids[&dead]);
                        self.model.nodes[dead].alive = false;
                        self.model
                            .links
                            .retain(|&(_, from, to)| from != dead && to != dead);
                    }
                }
            }

            writer.commit();

            if self.rng.chance(8) {
                writer.rebuild_now();
            }
        }

        if self.rng.chance(25) {
            writer.finish_rebuild();
        }

        if self.rng.chance(15) {
            writer.rebuild_and_wait();
        }
    }


    /// Deleted keys can come back, as new nodes.
    fn bring_one_back(&mut self) {
        let taken: BTreeSet<(usize, &str)> = self
            .model
            .nodes
            .iter()
            .filter(|node| node.alive)
            .map(|node| (node.node_type, node.key.as_str()))
            .collect();

        let gone = self
            .model
            .nodes
            .iter()
            .find(|node| !node.alive && !taken.contains(&(node.node_type, node.key.as_str())))
            .map(|node| (node.node_type, node.key.clone()));

        let Some((node_type, key)) = gone else {
            return;
        };

        let mut writer = self.store.writer();
        self.seq += 1;
        writer.begin(self.seq);

        let node = writer.add_node(TypeId(node_type as u16), key.as_bytes());
        let n = self.model.add(node_type, key, &mut self.rng);

        for (value, (name, _)) in self.model.nodes[n].values.iter().zip(PROPERTIES) {
            if let Some(cell) = value {
                writer.set_property(node, name, &cell.value());
            }
        }

        writer.commit();
    }


    fn check(&mut self) {
        let generation = self.store.current();
        let view = generation.view();

        assert_eq!(view.upto, self.seq, "seed {}: the sequence", self.seed);

        let mut ids = HashMap::new();
        let mut model_of = HashMap::new();

        for (n, node) in self.model.nodes.iter().enumerate() {
            if !node.alive {
                continue;
            }

            let g = view
                .lookup(TypeId(node.node_type as u16), node.key.as_bytes())
                .unwrap_or_else(|| panic!("seed {}: {} is missing", self.seed, node.key));

            assert_eq!(view.key(g), node.key.as_bytes(), "seed {}", self.seed);

            ids.insert(n, g);
            model_of.insert(g, n);
        }

        for _ in 0..QUESTIONS {
            self.ask(&view, &ids, &model_of);
        }
    }


    fn ask(
        &mut self,
        view: &View,
        ids: &HashMap<usize, GlobalId>,
        model_of: &HashMap<GlobalId, usize>,
    ) {
        let seed = self.seed;
        let seq = self.seq;
        let rng = &mut self.rng;
        let model = &self.model;
        let person = TypeId(0);

        // Conditions.
        let test = Test::random(rng, 3);
        let checked = condition::checked(view.graph, person, test.condition()).unwrap();
        let bits = condition::evaluate(view, person, &checked);

        for (&n, &g) in ids {
            if model.nodes[n].node_type == 0 {
                assert_eq!(
                    bits.get(view.get(g).1.index()),
                    test.holds(&model.nodes[n].values),
                    "seed {seed} seq {seq}: {test:?} on {} {:?}",
                    model.nodes[n].key,
                    model.nodes[n].values
                );
            }
        }

        let mut alive: Vec<usize> = ids.keys().copied().collect();
        alive.sort_unstable();

        if alive.is_empty() {
            return;
        }

        let starts: Vec<usize> = (0..1 + rng.below(3))
            .map(|_| alive[rng.below(alive.len())])
            .collect();
        let start_ids: Vec<GlobalId> = starts.iter().map(|n| ids[n]).collect();
        let chosen: Vec<usize> = match rng.below(4) {
            0 => vec![rng.below(3)],
            1 => vec![0, 1],
            _ => vec![0, 1, 2],
        };
        let link_types: Vec<LinkTypeId> = chosen
            .iter()
            .map(|&i| view.graph.link_type(LINKS[i].0).unwrap())
            .collect();
        let direction = [Direction::Forward, Direction::Backward, Direction::Both][rng.below(3)];
        let max = rng.below(7) as u32;
        let filtered = rng.chance(50);
        let others = rng.chance(60);

        let through = filtered.then(|| {
            let mut through = Through::new(view.graph, others);
            through.set(person, bits.clone());
            through
        });

        let allowed = |n: usize| match model.nodes[n].node_type {
            _ if !filtered => true,
            0 => test.holds(&model.nodes[n].values),
            _ => others,
        };

        let neighbours = model.neighbours(&chosen, direction);
        let context = format!(
            "seed {seed} seq {seq}: {direction:?} {chosen:?} max {max} filtered {filtered}"
        );

        // reach, on one thread and on several.
        let query = Reach {
            starts: &start_ids,
            link_types: &link_types,
            direction,
            max_steps: max,
            through: through.as_ref(),
            cap: None,
        };
        let expected = distances(&neighbours, &starts, max, true, &allowed);

        for pool in &self.pools {
            let found = pool.install(|| walk::reach(view, &query));

            assert_eq!(
                found.per_step.iter().sum::<u64>(),
                found.reached.len() as u64,
                "{context}: per_step"
            );

            let mut reached: Vec<(usize, u32)> = Vec::new();
            let mut rest = found.reached.as_slice();

            for (step, &count) in found.per_step.iter().enumerate() {
                let (these, later) = rest.split_at(count as usize);

                for g in these {
                    let n = *model_of
                        .get(g)
                        .unwrap_or_else(|| panic!("{context}: reached a deleted node"));
                    reached.push((n, step as u32 + 1));
                }

                rest = later;
            }

            reached.sort_unstable();

            let wanted: Vec<(usize, u32)> = expected
                .iter()
                .enumerate()
                .filter_map(|(n, &d)| d.filter(|&d| d > 0).map(|d| (n, d)))
                .collect();

            assert_eq!(
                reached,
                wanted,
                "{context}: reach, {} threads",
                pool.current_num_threads()
            );
        }

        // first_step.
        if max > 0 {
            let wanted = distances(&neighbours, &starts, 1, false, &allowed);
            let mut stepped: Vec<usize> = walk::first_step(view, &query)
                .iter()
                .map(|g| model_of[g])
                .collect();
            stepped.sort_unstable();

            let wanted: Vec<usize> = (0..wanted.len()).filter(|&n| wanted[n].is_some()).collect();
            assert_eq!(stepped, wanted, "{context}: first_step");
        }

        // Shortest paths. The end may always be stepped onto.
        let (from, to) = (starts[0], alive[rng.below(alive.len())]);
        let to_end = |n: usize| n == to || allowed(n);
        let shortest = distances(&neighbours, &[from], max, false, &to_end)[to];

        let length = walk::path_length(
            view,
            ids[&from],
            ids[&to],
            &link_types,
            direction,
            max,
            through.as_ref(),
        );
        assert_eq!(length, shortest, "{context}: path_length {from} to {to}");

        if from != to {
            let path = subgraph::shortest_path(
                view,
                ids[&from],
                ids[&to],
                &link_types,
                direction,
                max,
                through.as_ref(),
            );
            assert_eq!(
                path.as_ref().map(|p| p.links.len() as u32),
                shortest,
                "{context}: shortest_path"
            );

            for link in path.iter().flat_map(|path| &path.links) {
                let which = link_types
                    .iter()
                    .position(|&id| id == link.link_type)
                    .unwrap();
                let real = (chosen[which], model_of[&link.from], model_of[&link.to]);

                assert!(
                    model.links.contains(&real),
                    "{context}: the path uses a link that isn't there"
                );
            }
        }

        // Cypher, from one key or from every person meeting an equality. Undirected walks of several steps count
        // differently from Cypher (a known bug), so those aren't compared.
        let min = rng.below(3) as u32;
        let max = max.max(min);
        let start = starts[0];

        if model.nodes[start].node_type != 0 || (direction == Direction::Both && max > 1) {
            return;
        }

        let one = rng.chance(50);
        let banned = Test::Compare(4, Op::Eq, Cell::Bool(false));
        let from_set: Vec<usize> = if one {
            vec![start]
        } else {
            alive
                .iter()
                .copied()
                .filter(|&n| model.nodes[n].node_type == 0 && banned.holds(&model.nodes[n].values))
                .collect()
        };

        let head = if one {
            "(a:Person {key: $key})"
        } else {
            "(a:Person {banned: false})"
        };
        let (left, right) = match direction {
            Direction::Forward => ("-", "->"),
            Direction::Backward => ("<-", "-"),
            Direction::Both => ("-", "-"),
        };
        let names: Vec<&str> = chosen.iter().map(|&i| LINKS[i].0).collect();

        let active = Test::Compare(0, Op::Eq, Cell::Str("active".into()));
        let path_filter = if filtered {
            " (r, n | n.status = 'active')"
        } else {
            ""
        };
        let over = Test::Compare(1, Op::Gt, Cell::Num(1.0));
        let end_filter = rng.chance(50);
        let condition = if end_filter { " WHERE b.age > 1" } else { "" };

        let text = format!(
            "MATCH {head}{left}[:{}*{min}..{max}{path_filter}]{right}(b:Person){condition} RETURN b LIMIT 10000",
            names.join("|")
        );

        // The path filter passes types without its properties.
        let onto = |n: usize| {
            !filtered || model.nodes[n].node_type != 0 || active.holds(&model.nodes[n].values)
        };
        let mut distance = distances(&neighbours, &from_set, max, false, &onto);

        if min == 0 {
            from_set.iter().for_each(|&n| distance[n] = Some(0));
        }

        let wanted: BTreeSet<usize> = (0..model.nodes.len())
            .filter(|&n| {
                model.nodes[n].node_type == 0
                    && distance[n].is_some_and(|d| d >= min)
                    && (!end_filter || over.holds(&model.nodes[n].values))
            })
            .collect();

        let params = HashMap::from([(
            "key".to_string(),
            Value::Str(model.nodes[start].key.clone()),
        )]);
        let answer =
            cypher::run(view, &text, &params).unwrap_or_else(|e| panic!("{context}: {text}: {e}"));

        assert_eq!(answer.count, Some(wanted.len() as u64), "{context}: {text}");

        let drawing = answer.graph.unwrap();

        if drawing.truncated {
            return;
        }

        let matches: BTreeSet<usize> = drawing
            .nodes
            .iter()
            .filter(|(_, role)| *role == Role::Match)
            .map(|(g, _)| model_of[g])
            .collect();
        assert_eq!(matches, wanted, "{context}: drawn {text}");

        for link in &drawing.links {
            let link_type = LINKS
                .iter()
                .position(|(name, _, _)| *name == view.graph.link_types[link.link_type].name)
                .unwrap();
            let real = (link_type, model_of[&link.from], model_of[&link.to]);

            assert!(
                model.links.contains(&real),
                "{context}: a drawn link that isn't there"
            );

            for end in [link.from, link.to] {
                assert!(
                    drawing.nodes.iter().any(|&(node, _)| node == end),
                    "{context}: a link without its node"
                );
            }
        }
    }
}


// ---------------------------------------------------------------- the tests


#[test]
fn random_graphs_edits_and_questions() {
    let seeds = std::env::var("PROXIMADB_SEEDS")
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(SEEDS);

    // Either side of a bitset word and a SIMD step, down to almost nothing.
    let sizes = [1, 2, 8, 63, 64, 65, 127, 256, 300];

    for seed in 0..seeds {
        let mut run = Run::new(seed, sizes[seed as usize % sizes.len()]);

        run.check();

        for _ in 0..8 {
            run.edit();
            run.check();

            if run.rng.chance(40) {
                run.bring_one_back();
                run.check();
            }
        }

        run.store.writer().rebuild_and_wait();
        run.check();
    }
}


#[test]
fn graphs_big_enough_to_split_across_threads() {
    for seed in 0..3 {
        let mut run = Run::new(1_000 + seed, 2_500);

        run.check();
        run.edit();
        run.check();
        run.store.writer().rebuild_and_wait();
        run.check();
    }
}
