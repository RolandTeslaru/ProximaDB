//! Conditions on properties, turned into yes/no `Bitset`s over a node type's local ids:
//! the packed columns with SIMD (`evaluate_packed`), plus the changes since the rebuild (`evaluate`).

use std::simd::prelude::*;

use rustc_hash::{FxHashMap, FxHashSet};

pub use crate::core::Value;
use crate::kinetic::changes::{Val, decode, property_index};
use crate::kinetic::live::View;

use crate::collections::bitset::Bitset;
use crate::storage::graph::Graph;
use crate::storage::node_type;
use crate::storage::property::{Column, Property};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Debug)]
pub enum Condition {
    Compare {
        property: String,
        op: Op,
        value: Value,
    },
    HasValue(String),
    And(Vec<Condition>),
    Or(Vec<Condition>),
    Not(Box<Condition>),
}

impl Condition {
    pub fn compare(property: &str, op: Op, value: Value) -> Self {
        Condition::Compare {
            property: property.to_string(),
            op,
            value,
        }
    }
}

/// `condition` checked against `t`'s properties, ready to evaluate: every property exists, every value has the
/// property's kind (a date may be written as `YYYY-MM-DD`), and coded strings and booleans are only compared with `=`
/// and `<>`. Evaluating an unchecked condition from outside input could panic.
pub fn checked(graph: &Graph, t: node_type::Id, condition: Condition) -> Result<Condition, String> {
    let property_of = |name: &str| {
        graph
            .property(t, name)
            .ok_or_else(|| format!("{} has no property {name}", graph.node_types[t].name))
    };

    match condition {
        Condition::Compare {
            property: name,
            op,
            value,
        } => {
            let property = property_of(&name)?;
            let value = value_of_kind(property, value)?;

            let ordered = !matches!(
                property.column,
                Column::Coded8 { .. } | Column::Coded16 { .. } | Column::Boolean(_)
            );

            if !ordered && !matches!(op, Op::Eq | Op::Ne) {
                return Err(format!("{name} can only be compared with = and <>"));
            }

            Ok(Condition::Compare {
                property: name,
                op,
                value,
            })
        }

        Condition::HasValue(name) => {
            property_of(&name)?;

            Ok(Condition::HasValue(name))
        }

        Condition::And(parts) => Ok(Condition::And(checked_all(graph, t, parts)?)),

        Condition::Or(parts) => Ok(Condition::Or(checked_all(graph, t, parts)?)),

        Condition::Not(inner) => Ok(Condition::Not(Box::new(checked(graph, t, *inner)?))),
    }
}


fn checked_all(
    graph: &Graph,
    t: node_type::Id,
    parts: Vec<Condition>,
) -> Result<Vec<Condition>, String> {
    parts
        .into_iter()
        .map(|part| checked(graph, t, part))
        .collect()
}


fn value_of_kind(property: &Property, value: Value) -> Result<Value, String> {
    use crate::core::Kind;

    let name = &property.name;

    match (property.kind, value) {
        (Kind::String, value @ Value::Str(_)) | (Kind::Boolean, value @ Value::Bool(_)) => {
            Ok(value)
        }

        (Kind::Date, Value::Str(text)) => crate::storage::property::parse_date(&text)
            .map(|days| Value::Num(days as f64))
            .ok_or_else(|| format!("{name}: not a date: {text}")),

        (
            Kind::Integer | Kind::Long | Kind::Double | Kind::Date | Kind::Timestamp,
            value @ Value::Num(_),
        ) => Ok(value),

        (kind, _) => Err(format!(
            "{name} is a {kind:?} property: wrong type of value"
        )),
    }
}


/// The yes/no list of `condition` for every node of type `t`, indexed by `node::Id`. Missing values never match.
pub fn evaluate_packed(graph: &Graph, t: node_type::Id, condition: &Condition) -> Bitset {
    let n = graph.node_type_mappings.count(t) as usize;
    match condition {
        Condition::Compare {
            property,
            op,
            value,
        } => {
            let p = graph
                .property(t, property)
                .unwrap_or_else(|| panic!("no property {property}"));
            let mut out = compare(p, *op, value, n);
            out.and_assign(&p.has_value);
            out
        }
        Condition::HasValue(property) => graph
            .property(t, property)
            .expect("property")
            .has_value
            .clone(),
        Condition::And(parts) => {
            let mut out = all_ones(n);
            for c in parts {
                out.and_assign(&evaluate_packed(graph, t, c));
            }
            out
        }
        Condition::Or(parts) => {
            let mut out = Bitset::new(n);
            for c in parts {
                out.or_assign(&evaluate_packed(graph, t, c));
            }
            out
        }
        Condition::Not(c) => {
            let mut out = evaluate_packed(graph, t, c);
            out.not_assign();
            out
        }
    }
}

fn all_ones(n: usize) -> Bitset {
    let mut b = Bitset::new(n);
    b.not_assign();
    b
}

fn compare(p: &Property, op: Op, value: &Value, n: usize) -> Bitset {
    match (&p.column, value) {
        (Column::Coded8 { codes, .. }, Value::Str(s)) => match (op, p.code_of(s)) {
            (Op::Eq, Some(code)) => coded8_eq(codes, code as u8, false),
            (Op::Ne, Some(code)) => coded8_eq(codes, code as u8, true),
            (Op::Eq, None) => Bitset::new(n),
            (Op::Ne, None) => all_ones(n),
            _ => panic!("only = and != on coded strings"),
        },
        (Column::Coded16 { codes, .. }, Value::Str(s)) => {
            let code = p.code_of(s);
            match op {
                Op::Eq => scalar(n, |i| Some(codes[i]) == code),
                Op::Ne => scalar(n, |i| Some(codes[i]) != code),
                _ => panic!("only = and != on coded strings"),
            }
        }
        (Column::Text { bytes, starts }, Value::Str(s)) => {
            let get = |i: usize| &bytes[starts[i] as usize..starts[i + 1] as usize];
            match op {
                Op::Eq => scalar(n, |i| get(i) == s.as_bytes()),
                Op::Ne => scalar(n, |i| get(i) != s.as_bytes()),
                _ => scalar(n, |i| ordered(op, get(i).cmp(s.as_bytes()))),
            }
        }
        (Column::Double(v), Value::Num(x)) => double_cmp(v, op, *x),
        (Column::Integer(v), Value::Num(x)) => scalar(n, |i| num_cmp(op, v[i] as f64, *x)),
        (Column::Long(v), Value::Num(x)) => scalar(n, |i| num_cmp(op, v[i] as f64, *x)),
        (Column::Date(v), Value::Num(x)) => scalar(n, |i| num_cmp(op, v[i] as f64, *x)),
        (Column::Timestamp(v), Value::Num(x)) => scalar(n, |i| num_cmp(op, v[i] as f64, *x)),
        (Column::Boolean(b), Value::Bool(x)) => {
            let mut out = b.clone();
            if (op == Op::Eq) != *x {
                out.not_assign();
            }
            out
        }
        _ => panic!("condition value does not match the property's type"),
    }
}

fn ordered(op: Op, o: std::cmp::Ordering) -> bool {
    use std::cmp::Ordering::*;
    match op {
        Op::Eq => o == Equal,
        Op::Ne => o != Equal,
        Op::Lt => o == Less,
        Op::Le => o != Greater,
        Op::Gt => o == Greater,
        Op::Ge => o != Less,
    }
}

#[inline]
fn num_cmp(op: Op, a: f64, b: f64) -> bool {
    match op {
        Op::Eq => a == b,
        Op::Ne => a != b,
        Op::Lt => a < b,
        Op::Le => a <= b,
        Op::Gt => a > b,
        Op::Ge => a >= b,
    }
}

fn scalar(n: usize, f: impl Fn(usize) -> bool) -> Bitset {
    let mut words = vec![0u64; n.div_ceil(64)];
    for (w, word) in words.iter_mut().enumerate() {
        let base = w * 64;
        let mut bits = 0u64;
        for j in 0..64.min(n - base) {
            bits |= (f(base + j) as u64) << j;
        }
        *word = bits;
    }
    Bitset::from_words(words, n)
}

/// code == wanted (or !=), 32 nodes per comparison.
fn coded8_eq(codes: &[u8], wanted: u8, negate: bool) -> Bitset {
    let n = codes.len();
    let target = Simd::<u8, 32>::splat(wanted);
    let mut words = vec![0u64; n.div_ceil(64)];
    let full = n / 64;
    for w in 0..full {
        let lo = Simd::<u8, 32>::from_slice(&codes[w * 64..])
            .simd_eq(target)
            .to_bitmask();
        let hi = Simd::<u8, 32>::from_slice(&codes[w * 64 + 32..])
            .simd_eq(target)
            .to_bitmask();
        let bits = lo | (hi << 32);
        words[w] = if negate { !bits } else { bits };
    }
    for i in full * 64..n {
        if (codes[i] == wanted) != negate {
            words[i >> 6] |= 1 << (i & 63);
        }
    }
    Bitset::from_words(words, n)
}

/// value op x, 8 nodes per comparison.
fn double_cmp(values: &[f64], op: Op, x: f64) -> Bitset {
    let n = values.len();
    let xs = Simd::<f64, 8>::splat(x);
    let mut words = vec![0u64; n.div_ceil(64)];
    let full = n / 64;
    for w in 0..full {
        let mut bits = 0u64;
        for k in 0..8 {
            let v = Simd::<f64, 8>::from_slice(&values[w * 64 + k * 8..]);
            let m = match op {
                Op::Eq => v.simd_eq(xs),
                Op::Ne => v.simd_ne(xs),
                Op::Lt => v.simd_lt(xs),
                Op::Le => v.simd_le(xs),
                Op::Gt => v.simd_gt(xs),
                Op::Ge => v.simd_ge(xs),
            };
            bits |= m.to_bitmask() << (k * 8);
        }
        words[w] = bits;
    }
    for i in full * 64..n {
        if num_cmp(op, values[i], x) {
            words[i >> 6] |= 1 << (i & 63);
        }
    }
    Bitset::from_words(words, n)
}

// ---------------------------------------------------------------- with the changes since the rebuild

fn base_value(p: &Property, local: u64) -> Val {
    let i = local as usize;
    if i >= p.has_value.len() || !p.has_value.get(i) {
        return Val::Missing;
    }
    match &p.column {
        Column::Coded8 { values, codes } => Val::Str(values[codes[i] as usize].clone()),
        Column::Coded16 { values, codes } => Val::Str(values[codes[i] as usize].clone()),
        Column::Text { bytes, starts } => Val::Str(
            String::from_utf8_lossy(&bytes[starts[i] as usize..starts[i + 1] as usize])
                .into_owned(),
        ),
        Column::Integer(v) => Val::Num(v[i] as f64),
        Column::Long(v) => Val::Num(v[i] as f64),
        Column::Double(v) => Val::Num(v[i]),
        Column::Date(v) => Val::Num(v[i] as f64),
        Column::Timestamp(v) => Val::Num(v[i] as f64),
        Column::Boolean(b) => Val::Bool(b.get(i)),
    }
}

fn compare_value(op: Op, a: &Val, b: &Value) -> bool {
    use std::cmp::Ordering::*;
    let ord = match (a, b) {
        (Val::Missing, _) => return false,
        (Val::Num(x), Value::Num(y)) => x.partial_cmp(y),
        (Val::Str(x), Value::Str(y)) => Some(x.as_str().cmp(y.as_str())),
        (Val::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        _ => return false,
    };
    let Some(o) = ord else { return false };
    match op {
        Op::Eq => o == Equal,
        Op::Ne => o != Equal,
        Op::Lt => o == Less,
        Op::Le => o != Greater,
        Op::Gt => o == Greater,
        Op::Ge => o != Less,
    }
}

pub(crate) fn properties_in(c: &Condition, out: &mut Vec<String>) {
    match c {
        Condition::Compare { property, .. } | Condition::HasValue(property) => {
            out.push(property.clone())
        }
        Condition::And(parts) | Condition::Or(parts) => {
            parts.iter().for_each(|p| properties_in(p, out))
        }
        Condition::Not(inner) => properties_in(inner, out),
    }
}

/// The yes/no list of `condition` for every node of type `t` this view sees (local ids): the packed column scanned
/// with SIMD, then the nodes changed or added since the rebuild re-checked one by one.
pub fn evaluate(view: &View, t: node_type::Id, cond: &Condition) -> Bitset {
    let graph = view.graph;
    let mut out = evaluate_packed(graph, t, cond);
    let base = graph.node_type_mappings.count(t);
    let total = view.count(t);
    for _ in base..total {
        out.push(false);
    }
    // Latest value of each property touched, per node, as of this view.
    let mut names = Vec::new();
    properties_in(cond, &mut names);
    let mut latest: FxHashMap<(usize, u64), u64> = FxHashMap::default();
    let mut touched: FxHashSet<u64> = (base..total).collect();
    for name in &names {
        let pi = property_index(graph, t, name);
        let log = &view.live.property_log(t, pi).log;
        for i in 0..log.len() {
            let c = log.get(i).unwrap();
            if c.seq <= view.upto {
                latest.insert((pi, c.local), c.value);
                touched.insert(c.local);
            }
        }
    }
    for local in touched {
        let value_of = |name: &str| {
            let pi = property_index(graph, t, name);
            let p = &graph.node_types[t].properties[pi];
            match latest.get(&(pi, local)) {
                Some(&raw) => decode(p, view.live.property_log(t, pi), raw),
                None if local < base => base_value(p, local),
                None => Val::Missing,
            }
        };
        let yes = check(cond, &value_of);
        if yes {
            out.set(local as usize)
        } else {
            out.unset(local as usize)
        }
    }
    out
}

fn check(c: &Condition, value_of: &dyn Fn(&str) -> Val) -> bool {
    match c {
        Condition::Compare {
            property,
            op,
            value,
        } => compare_value(*op, &value_of(property), value),
        Condition::HasValue(property) => value_of(property) != Val::Missing,
        Condition::And(parts) => parts.iter().all(|p| check(p, value_of)),
        Condition::Or(parts) => parts.iter().any(|p| check(p, value_of)),
        Condition::Not(inner) => !check(inner, value_of),
    }
}
