//! Property columns: one array per property, per node type, indexed by `node::Id` (position within the type).

pub use crate::core::property::Kind;

use rustc_hash::FxHashMap;

use crate::collections::bitset::Bitset;

/// A property's values, sized by its Quarry type.
pub enum Column {
    /// String with few distinct values: the distinct values plus a 1-byte code per node.
    Coded8 {
        values: Vec<String>,
        codes: Vec<u8>,
    },
    /// String with up to 65,536 distinct values: a 2-byte code per node.
    Coded16 {
        values: Vec<String>,
        codes: Vec<u16>,
    },
    /// String with mostly unique values: bytes back to back plus where each starts.
    Text {
        bytes: Vec<u8>,
        starts: Vec<u64>,
    },
    Integer(Vec<i32>),
    Long(Vec<i64>),
    Double(Vec<f64>),
    Boolean(Bitset),
    /// Days since 1970-01-01.
    Date(Vec<i32>),
    /// Microseconds since 1970-01-01.
    Timestamp(Vec<i64>),
}

pub struct Property {
    pub name: String,
    pub kind: Kind,
    pub column: Column,
    /// "Has a value", one bit per node of the type.
    pub has_value: Bitset,
}

impl Property {
    pub fn bytes(&self) -> usize {
        let column = match &self.column {
            Column::Coded8 { values, codes } => {
                codes.capacity() + values.iter().map(|v| v.len() + 24).sum::<usize>()
            }
            Column::Coded16 { values, codes } => {
                codes.capacity() * 2 + values.iter().map(|v| v.len() + 24).sum::<usize>()
            }
            Column::Text { bytes, starts } => bytes.capacity() + starts.capacity() * 8,
            Column::Integer(v) => v.capacity() * 4,
            Column::Long(v) => v.capacity() * 8,
            Column::Double(v) => v.capacity() * 8,
            Column::Boolean(b) => b.bytes(),
            Column::Date(v) => v.capacity() * 4,
            Column::Timestamp(v) => v.capacity() * 8,
        };
        column + self.has_value.bytes()
    }

    /// The code of a string value in a coded column (None if no node has it).
    pub fn code_of(&self, value: &str) -> Option<u16> {
        match &self.column {
            Column::Coded8 { values, .. } | Column::Coded16 { values, .. } => {
                values.iter().position(|v| v == value).map(|p| p as u16)
            }
            _ => None,
        }
    }
}

/// Builds a column from text values (as read from a dataset), choosing the string shape by counting distinct values.
pub fn build(name: &str, kind: Kind, raw: &[Option<String>]) -> Property {
    let n = raw.len();
    let mut has_value = Bitset::new(n);
    for (i, v) in raw.iter().enumerate() {
        if v.is_some() {
            has_value.set(i);
        }
    }
    let column = match kind {
        Kind::String => build_string(raw),
        Kind::Integer => Column::Integer(
            raw.iter()
                .map(|v| v.as_deref().map_or(0, |s| s.parse().unwrap_or(0)))
                .collect(),
        ),
        Kind::Long => Column::Long(
            raw.iter()
                .map(|v| v.as_deref().map_or(0, |s| s.parse().unwrap_or(0)))
                .collect(),
        ),
        Kind::Double => Column::Double(
            raw.iter()
                .map(|v| v.as_deref().map_or(0.0, |s| s.parse().unwrap_or(0.0)))
                .collect(),
        ),
        Kind::Boolean => {
            let mut b = Bitset::new(n);
            for (i, v) in raw.iter().enumerate() {
                if matches!(v.as_deref(), Some("true") | Some("True") | Some("1")) {
                    b.set(i);
                }
            }
            Column::Boolean(b)
        }
        Kind::Date => Column::Date(
            raw.iter()
                .map(|v| v.as_deref().and_then(parse_date).unwrap_or(0))
                .collect(),
        ),
        Kind::Timestamp => Column::Timestamp(
            raw.iter()
                .map(|v| v.as_deref().and_then(parse_timestamp).unwrap_or(0))
                .collect(),
        ),
    };
    Property {
        name: name.to_string(),
        kind,
        column,
        has_value,
    }
}

fn build_string(raw: &[Option<String>]) -> Column {
    let mut index: FxHashMap<&str, u32> = FxHashMap::default();
    let mut values: Vec<String> = Vec::new();
    let mut codes: Vec<u32> = Vec::with_capacity(raw.len());
    for v in raw {
        let s = v.as_deref().unwrap_or("");
        let code = *index.entry(s).or_insert_with(|| {
            values.push(s.to_string());
            (values.len() - 1) as u32
        });
        codes.push(code);
        // More distinct values than a 2-byte code can hold: store the text.
        if values.len() > 65_536 {
            return text(raw);
        }
    }
    if values.len() * 2 > raw.len().max(2) {
        return text(raw);
    }
    if values.len() <= 256 {
        Column::Coded8 {
            values,
            codes: codes.into_iter().map(|c| c as u8).collect(),
        }
    } else {
        Column::Coded16 {
            values,
            codes: codes.into_iter().map(|c| c as u16).collect(),
        }
    }
}

fn text(raw: &[Option<String>]) -> Column {
    let mut bytes = Vec::new();
    let mut starts = Vec::with_capacity(raw.len() + 1);
    starts.push(0);
    for v in raw {
        bytes.extend_from_slice(v.as_deref().unwrap_or("").as_bytes());
        starts.push(bytes.len() as u64);
    }
    Column::Text { bytes, starts }
}

/// "YYYY-MM-DD" → days since 1970-01-01.
pub fn parse_date(s: &str) -> Option<i32> {
    let mut parts = s.get(..10)?.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next()?.parse().ok()?;
    let d: i64 = parts.next()?.parse().ok()?;
    Some(days_from_civil(y, m, d) as i32)
}

pub(crate) fn parse_timestamp(s: &str) -> Option<i64> {
    let days = parse_date(s)? as i64;
    let time = s.get(11..19).unwrap_or("00:00:00");
    let mut t = time.split(':').map(|p| p.parse::<i64>().unwrap_or(0));
    let seconds = t.next()? * 3600 + t.next()? * 60 + t.next()?;
    Some((days * 86_400 + seconds) * 1_000_000)
}

// Howard Hinnant's days_from_civil.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// One value as the rebuild carries it from the old columns and the change log into new columns.
#[derive(Clone, Debug, PartialEq)]
pub enum Cell {
    Str(String),
    Int(i64),
    F64(f64),
    Bool(bool),
}

/// The packed value of the node at `local`, if it has one.
pub fn cell(p: &Property, local: u64) -> Option<Cell> {
    let i = local as usize;
    if i >= p.has_value.len() || !p.has_value.get(i) {
        return None;
    }
    Some(match &p.column {
        Column::Coded8 { values, codes } => Cell::Str(values[codes[i] as usize].clone()),
        Column::Coded16 { values, codes } => Cell::Str(values[codes[i] as usize].clone()),
        Column::Text { bytes, starts } => Cell::Str(
            String::from_utf8_lossy(&bytes[starts[i] as usize..starts[i + 1] as usize])
                .into_owned(),
        ),
        Column::Integer(v) => Cell::Int(v[i] as i64),
        Column::Long(v) => Cell::Int(v[i]),
        Column::Double(v) => Cell::F64(v[i]),
        Column::Boolean(b) => Cell::Bool(b.get(i)),
        Column::Date(v) => Cell::Int(v[i] as i64),
        Column::Timestamp(v) => Cell::Int(v[i]),
    })
}

/// A written value as a cell of a property of `kind` (the same reading the writer's change log uses).
pub fn cell_from_value(kind: Kind, value: &crate::core::Value) -> Cell {
    use crate::core::Value;
    match (kind, value) {
        (Kind::String, Value::Str(s)) => Cell::Str(s.clone()),
        (Kind::Double, Value::Num(x)) => Cell::F64(*x),
        (Kind::Boolean, Value::Bool(b)) => Cell::Bool(*b),
        (Kind::Date, Value::Str(s)) => Cell::Int(parse_date(s).expect("date") as i64),
        (_, Value::Num(x)) => Cell::Int(*x as i64),
        _ => panic!("value does not match the property's type"),
    }
}

/// Builds a column from cells, choosing the string shape the same way `build` does.
pub fn build_cells(name: &str, kind: Kind, cells: &[Option<Cell>]) -> Property {
    let n = cells.len();
    let mut has_value = Bitset::new(n);
    for (i, c) in cells.iter().enumerate() {
        if c.is_some() {
            has_value.set(i);
        }
    }
    let int = |c: &Option<Cell>| match c {
        Some(Cell::Int(x)) => *x,
        _ => 0,
    };
    let column = match kind {
        Kind::String => {
            let raw: Vec<Option<String>> = cells
                .iter()
                .map(|c| {
                    if let Some(Cell::Str(s)) = c {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
                .collect();
            build_string(&raw)
        }
        Kind::Integer => Column::Integer(cells.iter().map(|c| int(c) as i32).collect()),
        Kind::Long => Column::Long(cells.iter().map(int).collect()),
        Kind::Date => Column::Date(cells.iter().map(|c| int(c) as i32).collect()),
        Kind::Timestamp => Column::Timestamp(cells.iter().map(int).collect()),
        Kind::Double => Column::Double(
            cells
                .iter()
                .map(|c| if let Some(Cell::F64(x)) = c { *x } else { 0.0 })
                .collect(),
        ),
        Kind::Boolean => {
            let mut b = Bitset::new(n);
            for (i, c) in cells.iter().enumerate() {
                if matches!(c, Some(Cell::Bool(true))) {
                    b.set(i);
                }
            }
            Column::Boolean(b)
        }
    };
    Property {
        name: name.to_string(),
        kind,
        column,
        has_value,
    }
}
