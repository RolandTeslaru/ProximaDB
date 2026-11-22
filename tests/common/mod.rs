//! What the integration tests share: temporary datasets, schemas, randomness and Cypher helpers.

#![allow(dead_code)]

#[cfg(feature = "server")]
pub mod server;

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arrow_array::{ArrayRef, RecordBatch};
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use proximadb::core::{Kind, Value};
use proximadb::load::{self, Backing, LinkTypeSpec, NodeTypeSpec, Schema};
use proximadb::query::cypher::{self, Answer};
use proximadb::query::subgraph::Role;
use proximadb::{Graph, View};

static NEXT: AtomicU64 = AtomicU64::new(0);


/// A directory of the test's own, removed when dropped. Its path is short enough to hold a Unix socket.
pub struct Dir(PathBuf);


impl Dir {
    pub fn new() -> Self {
        let number = NEXT.fetch_add(1, Ordering::Relaxed);
        let started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();

        let path = PathBuf::from(format!(
            "/tmp/pxdb-{}-{number}-{started}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();

        Self(path)
    }


    pub fn path(&self) -> &Path {
        &self.0
    }


    pub fn write(&self, name: &str, contents: impl AsRef<[u8]>) {
        fs::write(self.0.join(name), contents).unwrap();
    }


    pub fn parquet(&self, name: &str, columns: Vec<(&str, ArrayRef)>) {
        self.parquet_as(name, columns, Compression::SNAPPY);
    }


    pub fn parquet_as(&self, name: &str, columns: Vec<(&str, ArrayRef)>, codec: Compression) {
        let batch = RecordBatch::try_from_iter_with_nullable(
            columns.into_iter().map(|(name, array)| (name, array, true)),
        )
        .unwrap();

        let file = fs::File::create(self.0.join(name)).unwrap();
        let properties = WriterProperties::builder().set_compression(codec).build();
        let mut writer = ArrowWriter::try_new(file, batch.schema(), Some(properties)).unwrap();

        writer.write(&batch).unwrap();
        writer.close().unwrap();
    }
}


impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}


/// A reproducible random sequence (splitmix64).
pub struct Rng(pub u64);


impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);

        let mixed = (self.0 ^ (self.0 >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        let mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);

        mixed ^ (mixed >> 31)
    }


    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }


    pub fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
}


/// A node type keyed by its `key` column.
pub fn node_type(name: &str, file: &str, properties: &[(&str, Kind)]) -> NodeTypeSpec {
    NodeTypeSpec {
        name: name.into(),
        file: file.into(),
        primary_key: "key".into(),
        properties: properties
            .iter()
            .map(|&(name, kind)| (name.to_string(), kind))
            .collect(),
    }
}


/// A link type backed by a join dataset with columns `a` and `b`.
pub fn link_type(name: &str, from: &str, to: &str, file: &str) -> LinkTypeSpec {
    LinkTypeSpec {
        name: name.into(),
        from_type: from.into(),
        to_type: to.into(),
        backing: Backing::Dataset {
            file: file.into(),
            from_column: "a".into(),
            to_column: "b".into(),
        },
    }
}


/// People with one property, following each other.
pub fn people(
    property: &str,
    kind: Kind,
    rows: &[(&str, &str)],
    follows: &[(&str, &str)],
) -> Graph {
    let dir = Dir::new();

    let quoted = |text: &str| format!("\"{}\"", text.replace('"', "\"\""));
    let lines = |header: String, rows: &[(&str, &str)]| {
        rows.iter().fold(header, |text, (a, b)| {
            format!("{text}{},{}\n", quoted(a), quoted(b))
        })
    };

    dir.write("people.csv", lines(format!("key,{property}\n"), rows));
    dir.write("follows.csv", lines("a,b\n".into(), follows));

    let schema = Schema {
        node_types: vec![node_type("Person", "people.csv", &[(property, kind)])],
        link_types: vec![link_type("FOLLOWS", "Person", "Person", "follows.csv")],
    };

    load::load(dir.path(), &schema).unwrap()
}


pub fn no_params() -> HashMap<String, Value> {
    HashMap::new()
}


pub fn ask(view: &View, query: &str) -> Answer {
    cypher::run(view, query, &no_params()).unwrap_or_else(|error| panic!("{query}: {error}"))
}


pub fn key(view: &View, node: proximadb::core::node::GlobalId) -> String {
    String::from_utf8(view.key(node)).unwrap()
}


pub fn drawn(view: &View, answer: &Answer, role: Role) -> BTreeSet<String> {
    let graph = answer.graph.as_ref().expect("the query draws a graph");

    graph
        .nodes
        .iter()
        .filter(|(_, found)| *found == role)
        .map(|(node, _)| key(view, *node))
        .collect()
}


pub fn set(keys: &[&str]) -> BTreeSet<String> {
    keys.iter().map(|key| key.to_string()).collect()
}


pub fn column<A: arrow_array::Array + 'static>(array: A) -> ArrayRef {
    Arc::new(array)
}
