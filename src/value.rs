use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

/// The four physical column types supported by RustHouse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataType {
    Int64,
    Float64,
    Bool,
    String,
}

impl DataType {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_uppercase().as_str() {
            "INT64" => Some(Self::Int64),
            "FLOAT64" => Some(Self::Float64),
            "BOOL" | "BOOLEAN" => Some(Self::Bool),
            "STRING" => Some(Self::String),
            _ => None,
        }
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Int64 => "Int64",
            Self::Float64 => "Float64",
            Self::Bool => "Bool",
            Self::String => "String",
        })
    }
}

/// A scalar value read from or written to a typed column.
#[derive(Debug, Clone)]
pub enum Value {
    Int64(i64),
    Float64(f64),
    Bool(bool),
    String(String),
}

impl Value {
    #[must_use]
    pub fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }

    #[must_use]
    pub fn as_display_string(&self) -> String {
        match self {
            Self::Int64(value) => value.to_string(),
            Self::Float64(value) => format_float(*value),
            Self::Bool(value) => value.to_string(),
            Self::String(value) => value.clone(),
        }
    }

    pub(crate) fn sql_cmp(&self, other: &Self) -> Option<Ordering> {
        match (self, other) {
            (Self::Int64(left), Self::Int64(right)) => Some(left.cmp(right)),
            (Self::Float64(left), Self::Float64(right)) => left.partial_cmp(right),
            (Self::Int64(left), Self::Float64(right)) => (*left as f64).partial_cmp(right),
            (Self::Float64(left), Self::Int64(right)) => left.partial_cmp(&(*right as f64)),
            (Self::Bool(left), Self::Bool(right)) => Some(left.cmp(right)),
            (Self::String(left), Self::String(right)) => Some(left.cmp(right)),
            _ => None,
        }
    }

    fn variant_index(&self) -> u8 {
        match self {
            Self::Int64(_) => 0,
            Self::Float64(_) => 1,
            Self::Bool(_) => 2,
            Self::String(_) => 3,
        }
    }
}

fn float_cmp(left: f64, right: f64) -> Ordering {
    if left == right {
        Ordering::Equal
    } else {
        left.total_cmp(&right)
    }
}

fn canonical_float_bits(value: f64) -> u64 {
    if value == 0.0 { 0 } else { value.to_bits() }
}

fn format_float(value: f64) -> String {
    let rendered = value.to_string();
    if value.is_finite() && !rendered.contains(['.', 'e', 'E']) {
        format!("{rendered}.0")
    } else {
        rendered
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_display_string())
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Value {}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Value {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Int64(left), Self::Int64(right)) => left.cmp(right),
            (Self::Float64(left), Self::Float64(right)) => float_cmp(*left, *right),
            (Self::Bool(left), Self::Bool(right)) => left.cmp(right),
            (Self::String(left), Self::String(right)) => left.cmp(right),
            _ => self.variant_index().cmp(&other.variant_index()),
        }
    }
}

impl Hash for Value {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.variant_index().hash(state);
        match self {
            Self::Int64(value) => value.hash(state),
            Self::Float64(value) => canonical_float_bits(*value).hash(state),
            Self::Bool(value) => value.hash(state),
            Self::String(value) => value.hash(state),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_integral_floats_unambiguously() {
        assert_eq!(Value::Float64(2.0).as_display_string(), "2.0");
        assert_eq!(Value::Float64(2.5).as_display_string(), "2.5");
    }
}
