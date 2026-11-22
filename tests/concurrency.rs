//! Readers while the writer writes and rebuilds swap underneath them.

mod common;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use common::people;
use proximadb::core::{Direction, Kind, Value};
use proximadb::query::condition::{self, Condition, Op};
use proximadb::query::walk::{self, Reach};
use proximadb::storage::node_type::Id as TypeId;
use proximadb::{Graph, Store, View};

const PERSON: TypeId = TypeId(0);
const READERS: usize = 6;
const ACTIONS: u64 = 20_000;


fn pairs() -> Graph {
    let names: Vec<String> = (0..2_000).map(|i| format!("p{i}")).collect();
    let mut rows = vec![("hub", "off")];
    rows.extend(names.iter().map(|name| (name.as_str(), "off")));

    people("status", Kind::String, &rows, &[])
}


fn followed(view: &View) -> u64 {
    let hub = view.lookup(PERSON, b"hub").unwrap();
    let follows = [view.graph.link_type("FOLLOWS").unwrap()];

    let query = Reach {
        starts: &[hub],
        link_types: &follows,
        direction: Direction::Forward,
        max_steps: 1,
        through: None,
        cap: None,
    };

    walk::reach(view, &query).reached.len() as u64
}


fn switched_on(view: &View) -> u64 {
    let on = Condition::compare("status", Op::Eq, Value::Str("on".into()));

    condition::evaluate(view, PERSON, &on).count()
}


/// Action n follows one new pair and switches on the same pair. So at action n a view follows exactly 2n people and
/// sees exactly 2n switched on, never an odd number and never another action's count.
#[test]
fn every_view_is_one_moment() {
    let store = Store::with_limits(pairs(), 0.05, 40).with_catch_up(5);
    let done = AtomicBool::new(false);
    let reads = AtomicU64::new(0);

    std::thread::scope(|scope| {
        for _ in 0..READERS {
            scope.spawn(|| {
                let mut last = 0;

                while !done.load(Ordering::Relaxed) {
                    let generation = store.current();
                    let view = generation.view();

                    assert!(view.upto >= last, "went back from {last} to {}", view.upto);
                    last = view.upto;

                    let expected = 2 * view.upto.min(1_000);

                    assert_eq!(followed(&view), expected, "links at action {}", view.upto);
                    assert_eq!(
                        switched_on(&view),
                        expected,
                        "values at action {}",
                        view.upto
                    );

                    reads.fetch_add(1, Ordering::Relaxed);
                }
            });
        }

        // Dropped on a panic too, so the readers stop and the failure is reported.
        struct Stop<'a>(&'a AtomicBool);

        impl Drop for Stop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }

        let _stop = Stop(&done);
        let mut writer = store.writer();

        for seq in 1..=1_000u64 {
            let (hub, first, second, follows) = {
                let view = writer.generation().view();
                let find = |key: String| view.lookup(PERSON, key.as_bytes()).unwrap();

                (
                    find("hub".into()),
                    find(format!("p{}", 2 * seq - 2)),
                    find(format!("p{}", 2 * seq - 1)),
                    view.graph.link_type("FOLLOWS").unwrap(),
                )
            };

            writer.begin(seq);

            for person in [first, second] {
                writer.add_link(follows, hub, person);
                writer.set_property(person, "status", &Value::Str("on".into()));
            }

            writer.commit();

            std::thread::sleep(std::time::Duration::from_micros(100));
        }

        writer.rebuild_and_wait();
    });

    assert!(store.rebuilds() > 1, "no rebuild raced the readers");
    assert!(
        reads.load(Ordering::Relaxed) > 100,
        "the readers hardly read"
    );
    assert_eq!(followed(&store.current().view()), 2_000);
}


#[test]
fn an_old_view_answers_the_same_across_many_rebuilds() {
    let store = Store::with_limits(pairs(), 0.05, 100);
    let pinned = store.current();
    let before = pinned.view();

    let mut writer = store.writer();

    for seq in 1..=ACTIONS {
        let (person, hub, follows) = {
            let view = writer.generation().view();

            (
                view.lookup(PERSON, format!("p{}", seq % 2_000).as_bytes())
                    .unwrap(),
                view.lookup(PERSON, b"hub").unwrap(),
                view.graph.link_type("FOLLOWS").unwrap(),
            )
        };

        writer.begin(seq);
        writer.add_link(follows, hub, person);
        writer.set_property(person, "status", &Value::Str("on".into()));
        writer.commit();

        if seq % 2_000 == 0 {
            writer.rebuild_and_wait();

            assert_eq!(followed(&before), 0, "after {seq}");
            assert_eq!(switched_on(&before), 0, "after {seq}");
        }
    }
}
