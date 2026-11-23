//! Datasets as they really arrive: every Parquet type and codec, untidy CSV, missing and broken files.

mod common;

use arrow_array::{
    ArrayRef, BooleanArray, Date32Array, Date64Array, Decimal128Array, Float32Array, Float64Array,
    Int8Array, Int16Array, Int32Array, Int64Array, LargeStringArray, StringArray, StringViewArray,
    TimestampMicrosecondArray, TimestampMillisecondArray, TimestampNanosecondArray,
    TimestampSecondArray, UInt8Array, UInt16Array, UInt32Array, UInt64Array,
};
use common::{Dir, Rng, column, link_type, node_type};
use parquet::basic::{Compression, ZstdLevel};
use proximadb::core::{Kind, Value};
use proximadb::load::{self, Backing, LinkTypeSpec, NodeTypeSpec, Schema, Side};
use proximadb::query::condition::{self, Condition, Op};
use proximadb::storage::node_type::Id as TypeId;
use proximadb::{Draft, Graph, Store};

const PERSON: TypeId = TypeId(0);
const DAY_2024_02_29: i32 = 19_782;
const MICROS_2024_02_29: i64 = DAY_2024_02_29 as i64 * 86_400_000_000;


fn people(file: &str, properties: &[(&str, Kind)]) -> Schema {
    Schema {
        node_types: vec![node_type("Person", file, properties)],
        link_types: vec![],
    }
}


fn find(graph: &Graph, key: &str) -> bool {
    graph
        .key_index
        .get(
            &graph.keys,
            &graph.node_type_mappings,
            PERSON,
            key.as_bytes(),
        )
        .is_some()
}


fn matching(graph: &Graph, property: &str, value: Value) -> u64 {
    let condition = Condition::compare(property, Op::Eq, value);
    let condition = condition::checked(graph, PERSON, condition).unwrap();
    let draft = Draft::new(graph);

    condition::evaluate(&draft.view(graph), PERSON, &condition).count()
}


// ---------------------------------------------------------------- Parquet


#[test]
fn every_key_column_type() {
    let arrays: Vec<(&str, ArrayRef)> = vec![
        ("int8", column(Int8Array::from(vec![7, 42]))),
        ("int16", column(Int16Array::from(vec![7, 42]))),
        ("int32", column(Int32Array::from(vec![7, 42]))),
        ("int64", column(Int64Array::from(vec![7, 42]))),
        ("uint8", column(UInt8Array::from(vec![7, 42]))),
        ("uint16", column(UInt16Array::from(vec![7, 42]))),
        ("uint32", column(UInt32Array::from(vec![7, 42]))),
        ("uint64", column(UInt64Array::from(vec![7, 42]))),
        ("text", column(StringArray::from(vec!["7", "42"]))),
        (
            "large text",
            column(LargeStringArray::from(vec!["7", "42"])),
        ),
        ("text view", column(StringViewArray::from(vec!["7", "42"]))),
        (
            "dictionary",
            column(
                vec!["7", "42"]
                    .into_iter()
                    .collect::<arrow_array::DictionaryArray<arrow_array::types::Int32Type>>(),
            ),
        ),
    ];

    for (name, keys) in arrays {
        let dir = Dir::new();
        dir.parquet("people.parquet", vec![("key", keys)]);

        let graph = load::load(dir.path(), &people("people.parquet", &[])).unwrap();

        assert!(find(&graph, "7") && find(&graph, "42"), "{name}");
    }
}


#[test]
fn integer_key_extremes() {
    let numbers = [
        i64::MIN,
        i64::MIN + 1,
        -1,
        0,
        1,
        10,
        (1 << 53) + 1,
        i64::MAX,
    ];
    let dir = Dir::new();
    dir.parquet(
        "people.parquet",
        vec![("key", column(Int64Array::from(numbers.to_vec())))],
    );

    let graph = load::load(dir.path(), &people("people.parquet", &[])).unwrap();

    for number in numbers {
        assert!(find(&graph, &number.to_string()), "{number}");
    }
}


#[test]
fn every_value_column_type() {
    let day = DAY_2024_02_29;
    let micros = MICROS_2024_02_29;

    let cases: Vec<(&str, Kind, ArrayRef, Value)> = vec![
        (
            "int8",
            Kind::Integer,
            column(Int8Array::from(vec![Some(5), None])),
            Value::Num(5.0),
        ),
        (
            "int16",
            Kind::Long,
            column(Int16Array::from(vec![Some(5), None])),
            Value::Num(5.0),
        ),
        (
            "uint32",
            Kind::Long,
            column(UInt32Array::from(vec![Some(5), None])),
            Value::Num(5.0),
        ),
        (
            "int64",
            Kind::Long,
            column(Int64Array::from(vec![Some(-5), None])),
            Value::Num(-5.0),
        ),
        (
            "int32 as double",
            Kind::Double,
            column(Int32Array::from(vec![Some(5), None])),
            Value::Num(5.0),
        ),
        (
            "float32",
            Kind::Double,
            column(Float32Array::from(vec![Some(2.5), None])),
            Value::Num(2.5),
        ),
        (
            "float64",
            Kind::Double,
            column(Float64Array::from(vec![Some(2.5), None])),
            Value::Num(2.5),
        ),
        (
            "decimal",
            Kind::Double,
            column(
                Decimal128Array::from(vec![Some(250), None])
                    .with_precision_and_scale(10, 2)
                    .unwrap(),
            ),
            Value::Num(2.5),
        ),
        (
            "boolean",
            Kind::Boolean,
            column(BooleanArray::from(vec![Some(true), None])),
            Value::Bool(true),
        ),
        (
            "number as text",
            Kind::String,
            column(Int64Array::from(vec![Some(5), None])),
            Value::Str("5".into()),
        ),
        (
            "date32",
            Kind::Date,
            column(Date32Array::from(vec![Some(day), None])),
            Value::Num(day as f64),
        ),
        (
            "date64",
            Kind::Date,
            column(Date64Array::from(vec![Some(micros / 1_000), None])),
            Value::Num(day as f64),
        ),
        (
            "date as text",
            Kind::Date,
            column(StringArray::from(vec![Some("2024-02-29"), None])),
            Value::Str("2024-02-29".into()),
        ),
        (
            "seconds",
            Kind::Timestamp,
            column(TimestampSecondArray::from(vec![
                Some(micros / 1_000_000),
                None,
            ])),
            Value::Num(micros as f64),
        ),
        (
            "milliseconds",
            Kind::Timestamp,
            column(TimestampMillisecondArray::from(vec![
                Some(micros / 1_000),
                None,
            ])),
            Value::Num(micros as f64),
        ),
        (
            "microseconds",
            Kind::Timestamp,
            column(TimestampMicrosecondArray::from(vec![Some(micros), None])),
            Value::Num(micros as f64),
        ),
        (
            "nanoseconds",
            Kind::Timestamp,
            column(TimestampNanosecondArray::from(vec![
                Some(micros * 1_000),
                None,
            ])),
            Value::Num(micros as f64),
        ),
        (
            "with a time zone",
            Kind::Timestamp,
            column(
                TimestampMicrosecondArray::from(vec![Some(micros), None]).with_timezone("+02:00"),
            ),
            Value::Num(micros as f64),
        ),
    ];

    for (name, kind, values, wanted) in cases {
        let dir = Dir::new();
        dir.parquet(
            "people.parquet",
            vec![
                ("key", column(StringArray::from(vec!["ana", "ben"]))),
                ("value", values),
            ],
        );

        let graph = load::load(dir.path(), &people("people.parquet", &[("value", kind)])).unwrap();

        assert_eq!(
            graph.property(PERSON, "value").unwrap().has_value.count(),
            1,
            "{name}: null is no value"
        );
        assert_eq!(matching(&graph, "value", wanted), 1, "{name}");
    }
}


#[test]
fn every_supported_codec() {
    for codec in [
        Compression::UNCOMPRESSED,
        Compression::SNAPPY,
        Compression::ZSTD(ZstdLevel::default()),
        Compression::LZ4,
        Compression::LZ4_RAW,
    ] {
        let dir = Dir::new();
        let keys: Vec<String> = (0..10_000).map(|i| format!("person-{i}")).collect();
        dir.parquet_as(
            "people.parquet",
            vec![("key", column(StringArray::from(keys)))],
            codec,
        );

        let graph = load::load(dir.path(), &people("people.parquet", &[])).unwrap();

        assert_eq!(graph.node_count(), 10_000, "{codec:?}");
    }
}


#[test]
fn similar_and_awkward_column_names() {
    let names = ["key", "key2", "key_2", "Key", "with space", "prénom"];
    let dir = Dir::new();

    let columns = names
        .iter()
        .enumerate()
        .map(|(i, &name)| {
            (
                name,
                column(StringArray::from(vec![format!("in column {i}")])),
            )
        })
        .collect();
    dir.parquet("people.parquet", columns);

    for (i, &name) in names.iter().enumerate().skip(1) {
        let graph = load::load(
            dir.path(),
            &people("people.parquet", &[(name, Kind::String)]),
        )
        .unwrap();

        assert_eq!(
            matching(&graph, name, Value::Str(format!("in column {i}"))),
            1,
            "{name}"
        );
        assert!(find(&graph, "in column 0"), "{name}: the key column moved");
    }
}


#[test]
fn the_key_can_also_be_a_property() {
    let dir = Dir::new();
    dir.parquet(
        "people.parquet",
        vec![("key", column(Int64Array::from(vec![7, 42])))],
    );

    let graph = load::load(
        dir.path(),
        &people("people.parquet", &[("key", Kind::Long)]),
    )
    .unwrap();

    assert!(find(&graph, "42"));
    assert_eq!(matching(&graph, "key", Value::Num(42.0)), 1);
}


#[test]
fn an_empty_parquet_file() {
    let dir = Dir::new();
    dir.parquet(
        "people.parquet",
        vec![("key", column(StringArray::from(Vec::<&str>::new())))],
    );

    assert_eq!(
        load::load(dir.path(), &people("people.parquet", &[]))
            .unwrap()
            .node_count(),
        0
    );
}


/// Both formats, across several batches, with missing and repeated keys in every one: values stay on their rows.
#[test]
fn csv_and_parquet_agree_across_batches() {
    const ROWS: usize = 150_000;

    let dir = Dir::new();
    let mut rng = Rng(11);
    let mut keys = Vec::new();
    let mut values = Vec::new();
    let mut csv = String::from("key,value\n");
    let mut expected = std::collections::HashMap::new();

    for row in 0..ROWS {
        let key = match rng.below(20) {
            0 => None,
            1 if row > 0 => Some(format!("k{}", rng.below(row))),
            _ => Some(format!("k{row}")),
        };
        let value = (!rng.chance(10)).then_some(row as i64);

        csv += &format!(
            "{},{}\n",
            key.as_deref().unwrap_or(""),
            value.map_or(String::new(), |v| v.to_string())
        );

        if let Some(key) = &key {
            expected.entry(key.clone()).or_insert(value);
        }

        keys.push(key);
        values.push(value);
    }

    dir.write("people.csv", csv);
    dir.parquet(
        "people.parquet",
        vec![
            ("key", column(StringArray::from(keys))),
            ("value", column(Int64Array::from(values))),
        ],
    );

    for file in ["people.csv", "people.parquet"] {
        let (graph, report) =
            load::load_with_report(dir.path(), &people(file, &[("value", Kind::Long)])).unwrap();
        let seen = &report.node_types[0];

        assert_eq!(graph.node_count(), expected.len(), "{file}");
        assert_eq!(
            seen.nodes + seen.without_key + seen.duplicate_keys,
            ROWS as u64,
            "{file}"
        );

        let column = graph.property(PERSON, "value").unwrap();

        for (key, value) in &expected {
            let node = graph
                .key_index
                .get(
                    &graph.keys,
                    &graph.node_type_mappings,
                    PERSON,
                    key.as_bytes(),
                )
                .unwrap();
            let position = graph.node_type_mappings.get(node).1.0 as u64;

            let found = proximadb::storage::property::cell(column, position);
            let wanted = value.map(proximadb::storage::property::Cell::Int);

            assert_eq!(found, wanted, "{file}: {key}");
        }
    }
}


// ---------------------------------------------------------------- CSV


#[test]
fn untidy_csv_that_still_loads() {
    let cases: [(&str, &[u8]); 7] = [
        ("byte order mark", b"\xEF\xBB\xBFkey,v\nana,1\n"),
        ("Windows line ends", b"key,v\r\nana,1\r\n"),
        ("no newline at the end", b"key,v\nana,1"),
        ("blank lines", b"key,v\n\nana,1\n\n"),
        ("quoted header", b"\"key\",\"v\"\nana,1\n"),
        ("columns in another order", b"v,key\n1,ana\n"),
        ("columns not in the schema", b"x,key,y,v\n9,ana,9,1\n"),
    ];

    for (name, bytes) in cases {
        let dir = Dir::new();
        dir.write("people.csv", bytes);

        let graph = load::load(dir.path(), &people("people.csv", &[("v", Kind::Long)]))
            .unwrap_or_else(|e| panic!("{name}: {e}"));

        assert!(find(&graph, "ana"), "{name}");
        assert_eq!(matching(&graph, "v", Value::Num(1.0)), 1, "{name}");
    }
}


#[test]
fn unreadable_values_are_no_value() {
    let cases = [
        (Kind::Integer, "one"),
        (Kind::Integer, "1.5"),
        (Kind::Long, "99999999999999999999"),
        (Kind::Long, " 7"),
        (Kind::Double, "1,5"),
        (Kind::Boolean, "yes"),
        (Kind::Date, "31/01/2024"),
        (Kind::Date, "2024-1-3"),
        (Kind::Date, "日本語日本語"),
        (Kind::Timestamp, "yesterday"),
    ];

    for (kind, text) in cases {
        let dir = Dir::new();
        dir.write("people.csv", format!("key,v\nana,\"{text}\"\n"));

        let graph = load::load(dir.path(), &people("people.csv", &[("v", kind)])).unwrap();

        assert!(find(&graph, "ana"), "{text}: the row stays");
        assert_eq!(
            graph.property(PERSON, "v").unwrap().has_value.count(),
            0,
            "{kind:?} {text}"
        );
    }
}


#[test]
fn broken_datasets_are_errors_naming_the_file() {
    let cases: [(&str, Option<&[u8]>); 7] = [
        ("missing.csv", None),
        ("empty.csv", Some(b"")),
        ("no_key.csv", Some(b"id\nana\n")),
        ("ragged.csv", Some(b"key,v\nana\n")),
        ("not_utf8.csv", Some(b"key,v\n\xFF\xFE,1\n")),
        ("empty.parquet", Some(b"")),
        ("not.parquet", Some(b"key,v\nana,1\n")),
    ];

    for (file, bytes) in cases {
        let dir = Dir::new();

        if let Some(bytes) = bytes {
            dir.write(file, bytes);
        }

        let error = load::load(dir.path(), &people(file, &[]))
            .err()
            .unwrap_or_else(|| panic!("{file}: loaded"));

        assert!(error.to_string().contains(file), "{file}: {error}");
    }
}


#[test]
fn a_parquet_file_cut_short_is_an_error() {
    let dir = Dir::new();
    let keys: Vec<String> = (0..500).map(|i| format!("person-{i}")).collect();
    dir.parquet(
        "whole.parquet",
        vec![("key", column(StringArray::from(keys)))],
    );

    let whole = std::fs::read(dir.path().join("whole.parquet")).unwrap();

    // What a reload sees while a sync is still writing the file.
    for length in (0..whole.len()).step_by(7) {
        dir.write("cut.parquet", &whole[..length]);

        assert!(
            load::load(dir.path(), &people("cut.parquet", &[])).is_err(),
            "cut at {length}"
        );
    }
}


// ---------------------------------------------------------------- schemas


#[test]
fn foreign_keys_on_either_side_and_to_the_same_type() {
    let dir = Dir::new();
    dir.write(
        "people.csv",
        "key,manager,team\nana,,red\nben,ana,red\ncleo,cleo,blue\n",
    );
    dir.write("teams.csv", "key,lead\nred,ana\nblue,nobody\n");

    let foreign_key = |name: &str, to: &str, side, property: &str| LinkTypeSpec {
        name: name.into(),
        from_type: "Person".into(),
        to_type: to.into(),
        backing: Backing::ForeignKey {
            side,
            property: property.into(),
        },
    };

    let schema = Schema {
        node_types: vec![
            node_type("Person", "people.csv", &[]),
            node_type("Team", "teams.csv", &[]),
        ],
        link_types: vec![
            foreign_key("REPORTS_TO", "Person", Side::From, "manager"),
            foreign_key("LEADS", "Team", Side::To, "lead"),
            // The same column as both ends.
            foreign_key("IS", "Person", Side::From, "key"),
        ],
    };

    let (_, report) = load::load_with_report(dir.path(), &schema).unwrap();
    let seen: Vec<(u64, u64, u64)> = report
        .link_types
        .iter()
        .map(|l| (l.links, l.empty, l.dangling))
        .collect();

    assert_eq!(seen, [(2, 1, 0), (1, 0, 1), (3, 0, 0)]);
}


#[test]
fn unknown_types_in_a_schema_are_errors() {
    let dir = Dir::new();
    dir.write("people.csv", "key\nana\n");
    dir.write("links.csv", "a,b\n");

    let mut schema = people("people.csv", &[]);
    schema
        .link_types
        .push(link_type("KNOWS", "Person", "Nobody", "links.csv"));

    assert!(
        load::load(dir.path(), &schema)
            .err()
            .unwrap()
            .to_string()
            .contains("Nobody")
    );
}


#[test]
fn nothing_at_all() {
    let dir = Dir::new();
    let nothing = Schema {
        node_types: Vec::<NodeTypeSpec>::new(),
        link_types: vec![],
    };

    assert_eq!(load::load(dir.path(), &nothing).unwrap().node_count(), 0);
}


#[test]
fn types_that_start_empty_take_nodes_and_links() {
    let dir = Dir::new();
    dir.write("people.csv", "key\n");
    dir.write("things.csv", "key\n");
    dir.write("teams.csv", "key\nred\n");
    dir.write("owns.csv", "a,b\n");

    let schema = Schema {
        node_types: vec![
            node_type("Person", "people.csv", &[]),
            node_type("Thing", "things.csv", &[]),
            node_type("Team", "teams.csv", &[]),
        ],
        link_types: vec![link_type("OWNS", "Person", "Thing", "owns.csv")],
    };

    let store = Store::new(load::load(dir.path(), &schema).unwrap());
    let mut writer = store.writer();
    let owns = writer.generation().graph.link_type("OWNS").unwrap();

    writer.begin(1);
    let ana = writer.add_node(TypeId(0), b"ana");
    let phone = writer.add_node(TypeId(1), b"phone");
    writer.add_link(owns, ana, phone);
    writer.commit();
    writer.rebuild_and_wait();

    let generation = store.current();
    let count = common::ask(
        &generation.view(),
        "MATCH (t:Thing)<-[:OWNS]-(p:Person) RETURN count(p)",
    )
    .count;

    assert_eq!(count, Some(1));
}
