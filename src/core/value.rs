//! A value written to or compared with a property.

#[derive(Clone, Debug)]
pub enum Value {
    Str(String),
    Num(f64),
    Bool(bool),
}
