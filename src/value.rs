use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

/// A logical SQL type supported by RustHouse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataType {
    Int64,
    Float64,
    Bool,
    String,
    NullableInt64,
    NullableFloat64,
    NullableBool,
    NullableString,
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

    #[must_use]
    pub fn nullable(data_type: Self) -> Self {
        match data_type {
            Self::Int64 => Self::NullableInt64,
            Self::Float64 => Self::NullableFloat64,
            Self::Bool => Self::NullableBool,
            Self::String => Self::NullableString,
            Self::NullableInt64
            | Self::NullableFloat64
            | Self::NullableBool
            | Self::NullableString => data_type,
        }
    }

    #[must_use]
    pub fn is_nullable(&self) -> bool {
        matches!(
            self,
            Self::NullableInt64 | Self::NullableFloat64 | Self::NullableBool | Self::NullableString
        )
    }

    #[must_use]
    pub fn underlying_type(self) -> Self {
        match self {
            Self::NullableInt64 => Self::Int64,
            Self::NullableFloat64 => Self::Float64,
            Self::NullableBool => Self::Bool,
            Self::NullableString => Self::String,
            data_type => data_type,
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
            Self::NullableInt64 => "Nullable(Int64)",
            Self::NullableFloat64 => "Nullable(Float64)",
            Self::NullableBool => "Nullable(Bool)",
            Self::NullableString => "Nullable(String)",
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
    Null,
}

/// A non-owning scalar used while scanning immutable column storage.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ValueRef<'a> {
    Int64(i64),
    Float64(f64),
    Bool(bool),
    String(&'a str),
    Null,
}

impl Value {
    #[must_use]
    pub fn data_type(&self) -> Option<DataType> {
        match self {
            Self::Int64(_) => Some(DataType::Int64),
            Self::Float64(_) => Some(DataType::Float64),
            Self::Bool(_) => Some(DataType::Bool),
            Self::String(_) => Some(DataType::String),
            Self::Null => None,
        }
    }

    pub(crate) fn type_name(&self) -> String {
        self.data_type()
            .map_or_else(|| "NULL".to_owned(), |data_type| data_type.to_string())
    }

    #[must_use]
    pub fn as_display_string(&self) -> String {
        match self {
            Self::Int64(value) => value.to_string(),
            Self::Float64(value) => format_float(*value),
            Self::Bool(value) => value.to_string(),
            Self::String(value) => value.clone(),
            Self::Null => "NULL".to_owned(),
        }
    }

    pub(crate) fn as_ref(&self) -> ValueRef<'_> {
        match self {
            Self::Int64(value) => ValueRef::Int64(*value),
            Self::Float64(value) => ValueRef::Float64(*value),
            Self::Bool(value) => ValueRef::Bool(*value),
            Self::String(value) => ValueRef::String(value),
            Self::Null => ValueRef::Null,
        }
    }

    #[cfg(test)]
    pub(crate) fn sql_cmp(&self, other: &Self) -> Option<Ordering> {
        self.as_ref().sql_cmp(other.as_ref())
    }
}

impl ValueRef<'_> {
    pub(crate) fn to_owned(self) -> Value {
        match self {
            Self::Int64(value) => Value::Int64(value),
            Self::Float64(value) => Value::Float64(value),
            Self::Bool(value) => Value::Bool(value),
            Self::String(value) => Value::String(value.to_owned()),
            Self::Null => Value::Null,
        }
    }

    pub(crate) fn sql_cmp(self, other: Self) -> Option<Ordering> {
        match (self, other) {
            (Self::Null, _) | (_, Self::Null) => None,
            (Self::Int64(left), Self::Int64(right)) => Some(left.cmp(&right)),
            (Self::Float64(left), Self::Float64(right)) => left.partial_cmp(&right),
            (Self::Int64(left), Self::Float64(right)) => int_float_cmp(left, right),
            (Self::Float64(left), Self::Int64(right)) => {
                int_float_cmp(right, left).map(Ordering::reverse)
            }
            (Self::Bool(left), Self::Bool(right)) => Some(left.cmp(&right)),
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
            Self::Null => 4,
        }
    }
}

fn int_float_cmp(integer: i64, float: f64) -> Option<Ordering> {
    const I64_UPPER_EXCLUSIVE: f64 = 9_223_372_036_854_775_808.0;

    if float.is_nan() {
        return None;
    }
    if float >= I64_UPPER_EXCLUSIVE {
        return Some(Ordering::Less);
    }
    if float < i64::MIN as f64 {
        return Some(Ordering::Greater);
    }

    let truncated = float.trunc() as i64;
    match integer.cmp(&truncated) {
        Ordering::Equal => (truncated as f64).partial_cmp(&float),
        ordering => Some(ordering),
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
        self.as_ref().cmp(&other.as_ref())
    }
}

impl PartialEq for ValueRef<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for ValueRef<'_> {}

impl PartialOrd for ValueRef<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ValueRef<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Int64(left), Self::Int64(right)) => left.cmp(right),
            (Self::Float64(left), Self::Float64(right)) => float_cmp(*left, *right),
            (Self::Bool(left), Self::Bool(right)) => left.cmp(right),
            (Self::String(left), Self::String(right)) => left.cmp(right),
            (Self::Null, Self::Null) => Ordering::Equal,
            _ => self.variant_index().cmp(&other.variant_index()),
        }
    }
}

impl Hash for Value {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_ref().hash(state);
    }
}

impl Hash for ValueRef<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.variant_index().hash(state);
        match self {
            Self::Int64(value) => value.hash(state),
            Self::Float64(value) => canonical_float_bits(*value).hash(state),
            Self::Bool(value) => value.hash(state),
            Self::String(value) => value.hash(state),
            Self::Null => {}
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

    #[test]
    fn compares_mixed_numbers_without_losing_integer_precision() {
        let beyond_exact_f64 = Value::Int64(9_007_199_254_740_993);
        let rounded_float = Value::Float64(9_007_199_254_740_992.0);
        assert_eq!(
            beyond_exact_f64.sql_cmp(&rounded_float),
            Some(Ordering::Greater)
        );
        assert_eq!(
            rounded_float.sql_cmp(&beyond_exact_f64),
            Some(Ordering::Less)
        );

        assert_eq!(
            Value::Int64(i64::MAX).sql_cmp(&Value::Float64(9_223_372_036_854_775_808.0)),
            Some(Ordering::Less)
        );
        assert_eq!(
            Value::Int64(i64::MAX).sql_cmp(&Value::Float64(9_223_372_036_854_774_784.0)),
            Some(Ordering::Greater)
        );
        assert_eq!(
            Value::Int64(i64::MIN).sql_cmp(&Value::Float64(-9_223_372_036_854_775_808.0)),
            Some(Ordering::Equal)
        );
        assert_eq!(
            Value::Int64(-1).sql_cmp(&Value::Float64(-1.5)),
            Some(Ordering::Greater)
        );
    }
}
