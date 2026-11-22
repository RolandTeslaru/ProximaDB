//! Replacing the packed graph, as a reload does.

use proximadb::load::{self, Schema};
use proximadb::storage::node_type;
use proximadb::{Graph, Store};


fn graph() -> Graph {
    let schema = Schema {
        node_types: vec![],
        link_types: vec![],
    };

    let mut graph = load::load(&std::env::temp_dir(), &schema).unwrap();

    graph.node_types.push(node_type::Storage {
        name: "Person".into(),
        primary_key: "key".into(),
        properties: vec![],
    });
    graph.node_type_mappings = node_type::Map::new(1);

    graph
}


#[test]
fn searches_wait_for_the_whole_replay() {
    let store = Store::with_limits(graph(), 0.05, 0);
    let before = store.current();
    let mut writer = store.writer();

    writer.replace(graph(), 0);

    // Enough edits to start a rebuild, and enough time for it to finish, all during the replay.
    for seq in 1..=20 {
        writer.begin(seq);
        writer.add_node(node_type::Id(0), seq.to_string().as_bytes());
        writer.commit();

        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    assert!(
        std::sync::Arc::ptr_eq(&before, &store.current()),
        "searches moved to the new graph before make_current"
    );

    writer.make_current();

    assert_eq!(store.current().view().node_count(), 20);
}
