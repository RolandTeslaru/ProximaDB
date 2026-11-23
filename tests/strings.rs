//! Strings nobody plans for, as keys and as values, through every way in and out: CSV, Parquet, actions, rebuilds,
//! Cypher literals and parameters.

mod common;

use std::collections::{HashMap, HashSet};

use arrow_array::StringArray;
use common::{Dir, column, link_type, node_type};
use proximadb::core::{Kind, Value};
use proximadb::load::{self, Schema};
use proximadb::query::condition::{self, Condition, Op};
use proximadb::query::cypher;
use proximadb::storage::node_type::Id as TypeId;
use proximadb::storage::property::Column;
use proximadb::{Draft, Graph, Store, View};

const PERSON: TypeId = TypeId(0);


/// Every one must stay itself: none may be trimmed, normalised, truncated, split or taken for another.
fn weird() -> Vec<String> {
    let mut all: Vec<String> = [
        "plain",
        "Plain",
        "PLAIN",
        " plain",
        "plain ",
        " ",
        "   ",
        "\t",
        "\n",
        "\r\n",
        "two\nlines",
        "nul\0inside",
        "\0",
        "comma,inside",
        "semi;colon",
        "\"",
        "\"quoted\"",
        "'",
        "it's",
        "''",
        "\\",
        "\\'",
        "\\n",
        "trailing\\",
        "`backtick`",
        "$who",
        "{a: 1}",
        "Person:ana",
        "a/b?c=d&e#f",
        "%00",
        "../../etc/passwd",
        "'; DROP TABLE people; --",
        "ana'}) RETURN a //",
        "<script>alert(1)</script>",
        "null",
        "NULL",
        "None",
        "true",
        "NaN",
        "0",
        "00",
        "007",
        "-0",
        "1e3",
        "1.0",
        "9007199254740993",
        "99999999999999999999999999999",
        "caf\u{e9}",   // composed
        "cafe\u{301}", // the same text, decomposed
        "stra\u{df}e",
        "strasse",
        "\u{130}stanbul",
        "\u{219}i \u{21b}ar\u{103}", // Romanian comma-below
        "\u{15f}i",                  // and the cedilla look-alike
        "\u{430}na",                 // a Cyrillic a
        "ana",
        "\u{65e5}\u{672c}\u{8a9e}",
        "\u{627}\u{644}\u{639}\u{631}\u{628}\u{64a}\u{629}",
        "\u{1f600}",
        "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}",
        "\u{feff}bom",
        "zero\u{200b}width",
        "no\u{a0}break",
        "line\u{2028}separator",
        "right\u{202e}to left",
        "\u{85}",
        "\u{7f}",
        "\u{fffd}",
        "\u{10ffff}",
    ]
    .iter()
    .map(|text| text.to_string())
    .collect();

    // Lengths either side of one, two and three byte boundaries.
    for length in [255, 256, 257, 4_096, 65_535, 65_536, 65_537] {
        all.push("x".repeat(length));
    }

    all.push("\u{1f600}".repeat(10_000));

    all
}


/// Each weird string is a node's key and that node's `value`, and links to the next one.
fn schema(extension: &str) -> Schema {
    Schema {
        node_types: vec![node_type(
            "Person",
            &format!("people.{extension}"),
            &[("value", Kind::String)],
        )],
        link_types: vec![link_type(
            "NEXT",
            "Person",
            "Person",
            &format!("next.{extension}"),
        )],
    }
}


fn from_csv(strings: &[String]) -> Graph {
    let dir = Dir::new();
    let field = |text: &str| format!("\"{}\"", text.replace('"', "\"\""));

    let mut people = String::from("key,value\n");
    let mut next = String::from("a,b\n");

    for (i, text) in strings.iter().enumerate() {
        let after = &strings[(i + 1) % strings.len()];

        people += &format!("{},{}\n", field(text), field(text));
        next += &format!("{},{}\n", field(text), field(after));
    }

    dir.write("people.csv", people);
    dir.write("next.csv", next);

    load::load(dir.path(), &schema("csv")).unwrap()
}


fn from_parquet(strings: &[String]) -> Graph {
    let dir = Dir::new();
    let texts: Vec<&str> = strings.iter().map(String::as_str).collect();
    let mut after = texts.clone();
    after.rotate_left(1);

    dir.parquet(
        "people.parquet",
        vec![
            ("key", column(StringArray::from(texts.clone()))),
            ("value", column(StringArray::from(texts.clone()))),
        ],
    );
    dir.parquet(
        "next.parquet",
        vec![
            ("a", column(StringArray::from(texts))),
            ("b", column(StringArray::from(after))),
        ],
    );

    load::load(dir.path(), &schema("parquet")).unwrap()
}


fn by_actions(strings: &[String]) -> Store {
    let dir = Dir::new();
    dir.write("people.csv", "key,value\n");
    dir.write("next.csv", "a,b\n");

    let store = Store::new(load::load(dir.path(), &schema("csv")).unwrap());

    {
        let mut writer = store.writer();
        let next = writer.generation().graph.link_type("NEXT").unwrap();

        writer.begin(1);

        let nodes: Vec<_> = strings
            .iter()
            .map(|text| {
                let node = writer.add_node(PERSON, text.as_bytes());
                writer.set_property(node, "value", &Value::Str(text.clone()));
                node
            })
            .collect();

        for (i, &node) in nodes.iter().enumerate() {
            writer.add_link(next, node, nodes[(i + 1) % nodes.len()]);
        }

        writer.commit();
    }

    store
}


/// Single-quoted, with only the two escapes a literal needs.
fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
}


fn every_string_is_itself(view: &View, strings: &[String], way_in: &str) {
    for (i, text) in strings.iter().enumerate() {
        let shown: String = text.chars().take(30).collect();
        let context = format!("{way_in}, string {i} {shown:?}");

        let node = view
            .lookup(PERSON, text.as_bytes())
            .unwrap_or_else(|| panic!("{context}: not found"));
        assert_eq!(view.key(node), text.as_bytes(), "{context}: key changed");

        let equal = Condition::compare("value", Op::Eq, Value::Str(text.clone()));
        let found: Vec<usize> = condition::evaluate(view, PERSON, &equal).ones().collect();
        assert_eq!(
            found,
            [view.get(node).1.index()],
            "{context}: value matches exactly one node"
        );

        let unequal = Condition::compare("value", Op::Ne, Value::Str(text.clone()));
        let others = condition::evaluate(view, PERSON, &unequal).count();
        assert_eq!(
            others as usize,
            strings.len() - 1,
            "{context}: <> is every other node"
        );

        let after = &strings[(i + 1) % strings.len()];
        let params = HashMap::from([("who".to_string(), Value::Str(text.clone()))]);

        let queries = [
            "MATCH (a:Person {key: $who})-[:NEXT]->(b:Person) RETURN b".to_string(),
            format!(
                "MATCH (a:Person {{key: {}}})-[:NEXT]->(b:Person) RETURN b",
                literal(text)
            ),
        ];

        for query in queries {
            let answer =
                cypher::run(view, &query, &params).unwrap_or_else(|e| panic!("{context}: {e}"));
            let next = view.lookup(PERSON, after.as_bytes()).unwrap();

            assert_eq!(answer.count, Some(1), "{context}: one next");
            assert!(
                answer
                    .graph
                    .unwrap()
                    .nodes
                    .iter()
                    .any(|&(found, _)| found == next),
                "{context}: the wrong next"
            );
        }
    }
}


#[test]
fn the_list_has_no_repeats() {
    let all = weird();

    assert_eq!(all.iter().collect::<HashSet<_>>().len(), all.len());
}


#[test]
fn weird_strings_through_csv() {
    let strings = weird();
    let graph = from_csv(&strings);
    let draft = Draft::new(&graph);

    every_string_is_itself(&draft.view(&graph), &strings, "CSV");
}


#[test]
fn weird_strings_through_parquet() {
    let strings = weird();
    let graph = from_parquet(&strings);
    let draft = Draft::new(&graph);

    every_string_is_itself(&draft.view(&graph), &strings, "Parquet");
}


#[test]
fn weird_strings_through_actions_and_rebuilds() {
    let strings = weird();
    let store = by_actions(&strings);

    every_string_is_itself(&store.current().view(), &strings, "actions");

    store.writer().rebuild_and_wait();
    every_string_is_itself(&store.current().view(), &strings, "actions, rebuilt");

    store.writer().rebuild_and_wait();
    every_string_is_itself(&store.current().view(), &strings, "actions, rebuilt twice");
}


#[test]
fn weird_strings_as_coded_values() {
    let strings = weird();
    let dir = Dir::new();
    let field = |text: &str| format!("\"{}\"", text.replace('"', "\"\""));

    // Each value three times, so the column is coded instead of text.
    let mut people = String::from("key,value\n");

    for (i, text) in strings.iter().enumerate() {
        for copy in 0..3 {
            people += &format!("{i}-{copy},{}\n", field(text));
        }
    }

    dir.write("people.csv", people);
    dir.write("next.csv", "a,b\n");

    let graph = load::load(dir.path(), &schema("csv")).unwrap();
    assert!(matches!(
        graph.property(PERSON, "value").unwrap().column,
        Column::Coded8 { .. }
    ));

    let draft = Draft::new(&graph);
    let view = draft.view(&graph);

    for text in &strings {
        let equal = Condition::compare("value", Op::Eq, Value::Str(text.clone()));

        assert_eq!(condition::evaluate(&view, PERSON, &equal).count(), 3);
    }
}


#[test]
fn every_column_shape_boundary() {
    // Distinct values either side of what a 1-byte and a 2-byte code can hold.
    for distinct in [1, 2, 3, 255, 256, 257, 65_535, 65_536, 65_537] {
        let dir = Dir::new();
        let mut people = String::from("key,value\n");

        for i in 0..distinct * 2 {
            people += &format!("k{i},v{}\n", i % distinct);
        }

        dir.write("people.csv", people);
        dir.write("next.csv", "a,b\n");

        let graph = load::load(dir.path(), &schema("csv")).unwrap();

        let shape = match graph.property(PERSON, "value").unwrap().column {
            Column::Coded8 { .. } => 8,
            Column::Coded16 { .. } => 16,
            Column::Text { .. } => 0,
            _ => panic!("not a string column"),
        };

        let expected = match distinct {
            ..=256 => 8,
            257..=65_536 => 16,
            _ => 0,
        };
        assert_eq!(shape, expected, "{distinct} distinct values");

        let draft = Draft::new(&graph);
        let view = draft.view(&graph);

        for wanted in [0, distinct / 2, distinct - 1] {
            let equal = Condition::compare("value", Op::Eq, Value::Str(format!("v{wanted}")));

            assert_eq!(
                condition::evaluate(&view, PERSON, &equal).count(),
                2,
                "{distinct}: v{wanted}"
            );
        }

        let absent = Condition::compare("value", Op::Ne, Value::Str("absent".into()));
        assert_eq!(
            condition::evaluate(&view, PERSON, &absent).count() as usize,
            distinct * 2
        );
    }
}


#[test]
fn a_parameter_is_data_never_query_text() {
    let strings = weird();
    let graph = from_csv(&strings);
    let draft = Draft::new(&graph);
    let view = draft.view(&graph);

    for attack in ["x' OR a.value <> '", "plain'}) RETURN a //", "$who"] {
        let params = HashMap::from([("who".to_string(), Value::Str(attack.into()))]);
        let answer = cypher::run(
            &view,
            "MATCH (a:Person) WHERE a.value = $who RETURN count(a)",
            &params,
        )
        .unwrap();

        let expected = strings.iter().filter(|text| *text == attack).count() as u64;
        assert_eq!(answer.count, Some(expected), "{attack}");
    }
}


/// Every Unicode scalar where a query can hold one: it answers or refuses, and never hangs or panics.
#[test]
fn every_unicode_character_in_a_query() {
    let strings = ["ana".to_string()];
    let graph = from_csv(&strings);
    let draft = Draft::new(&graph);
    let view = draft.view(&graph);
    let params = HashMap::new();

    for c in (0..=0x10_FFFF).filter_map(char::from_u32) {
        for query in [
            format!("MATCH ({c}"),
            format!("MATCH (a{c}"),
            format!("MATCH (a:Person {{key: '{c}"),
            format!("MATCH (a) WHERE a.x = ${c}"),
        ] {
            assert!(cypher::run(&view, &query, &params).is_err(), "{query:?}");
        }
    }
}
