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
    Null,
    Int64(i64),
    Float64(f64),
    Bool(bool),
    String(String),
}

/// A non-owning scalar used while scanning immutable column storage.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ValueRef<'a> {
    Null,
    Int64(i64),
    Float64(f64),
    Bool(bool),
    String(&'a str),
}

/// A hash key with SQL equality semantics.
///
/// Unlike `ValueRef`'s total ordering, this treats every NULL as one key and
/// exactly equal Int64/Float64 values as the same key.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SqlKeyRef<'a>(ValueRef<'a>);

impl Value {
    #[must_use]
    pub fn data_type(&self) -> Option<DataType> {
        match self {
            Self::Null => None,
            Self::Int64(_) => Some(DataType::Int64),
            Self::Float64(_) => Some(DataType::Float64),
            Self::Bool(_) => Some(DataType::Bool),
            Self::String(_) => Some(DataType::String),
        }
    }

    #[must_use]
    pub fn as_display_string(&self) -> String {
        match self {
            Self::Null => "NULL".to_owned(),
            Self::Int64(value) => value.to_string(),
            Self::Float64(value) => format_float(*value),
            Self::Bool(value) => value.to_string(),
            Self::String(value) => value.clone(),
        }
    }

    pub(crate) fn as_ref(&self) -> ValueRef<'_> {
        match self {
            Self::Null => ValueRef::Null,
            Self::Int64(value) => ValueRef::Int64(*value),
            Self::Float64(value) => ValueRef::Float64(*value),
            Self::Bool(value) => ValueRef::Bool(*value),
            Self::String(value) => ValueRef::String(value),
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
            Self::Null => Value::Null,
            Self::Int64(value) => Value::Int64(value),
            Self::Float64(value) => Value::Float64(value),
            Self::Bool(value) => Value::Bool(value),
            Self::String(value) => Value::String(value.to_owned()),
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
            Self::Null => 0,
            Self::Int64(_) => 1,
            Self::Float64(_) => 2,
            Self::Bool(_) => 3,
            Self::String(_) => 4,
        }
    }
}

impl<'a> SqlKeyRef<'a> {
    pub(crate) fn new(value: ValueRef<'a>) -> Self {
        Self(value)
    }

    pub(crate) fn value(self) -> ValueRef<'a> {
        self.0
    }
}

impl PartialEq for SqlKeyRef<'_> {
    fn eq(&self, other: &Self) -> bool {
        match (self.0, other.0) {
            (ValueRef::Null, ValueRef::Null) => true,
            (ValueRef::Int64(left), ValueRef::Int64(right)) => left == right,
            (ValueRef::Float64(left), ValueRef::Float64(right)) => {
                float_cmp(left, right) == Ordering::Equal
            }
            (ValueRef::Int64(left), ValueRef::Float64(right))
            | (ValueRef::Float64(right), ValueRef::Int64(left)) => {
                int_float_cmp(left, right) == Some(Ordering::Equal)
            }
            (ValueRef::Bool(left), ValueRef::Bool(right)) => left == right,
            (ValueRef::String(left), ValueRef::String(right)) => left == right,
            _ => false,
        }
    }
}

impl Eq for SqlKeyRef<'_> {}

impl Hash for SqlKeyRef<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self.0 {
            ValueRef::Null => 0_u8.hash(state),
            ValueRef::Int64(value) => {
                1_u8.hash(state);
                0_u8.hash(state);
                value.hash(state);
            }
            ValueRef::Float64(value) => {
                1_u8.hash(state);
                if let Some(integer) = exact_i64(value) {
                    0_u8.hash(state);
                    integer.hash(state);
                } else {
                    1_u8.hash(state);
                    canonical_float_bits(value).hash(state);
                }
            }
            ValueRef::Bool(value) => {
                2_u8.hash(state);
                value.hash(state);
            }
            ValueRef::String(value) => {
                3_u8.hash(state);
                value.hash(state);
            }
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

fn exact_i64(value: f64) -> Option<i64> {
    if !value.is_finite() || value >= 9_223_372_036_854_775_808.0 || value < i64::MIN as f64 {
        return None;
    }
    let integer = value as i64;
    (int_float_cmp(integer, value) == Some(Ordering::Equal)).then_some(integer)
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
            (Self::Null, Self::Null) => Ordering::Equal,
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
        self.as_ref().hash(state);
    }
}

impl Hash for ValueRef<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.variant_index().hash(state);
        match self {
            Self::Null => {}
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

    #[test]
    fn sql_hash_keys_match_null_and_exact_mixed_numeric_equality() {
        use std::collections::hash_map::DefaultHasher;

        fn hash(value: ValueRef<'_>) -> u64 {
            let mut hasher = DefaultHasher::new();
            SqlKeyRef::new(value).hash(&mut hasher);
            hasher.finish()
        }

        let integer = SqlKeyRef::new(ValueRef::Int64(9_007_199_254_740_992));
        let equal_float = SqlKeyRef::new(ValueRef::Float64(9_007_199_254_740_992.0));
        let distinct_integer = SqlKeyRef::new(ValueRef::Int64(9_007_199_254_740_993));
        assert_eq!(integer, equal_float);
        assert_eq!(hash(integer.0), hash(equal_float.0));
        assert_ne!(distinct_integer, equal_float);
        assert_eq!(
            SqlKeyRef::new(ValueRef::Null),
            SqlKeyRef::new(ValueRef::Null)
        );
    }

    #[test]
    fn sql_hash_key_equality_survives_complete_hash_collisions() {
        use std::collections::HashSet;
        use std::hash::BuildHasherDefault;

        #[derive(Default)]
        struct ConstantHasher;

        impl Hasher for ConstantHasher {
            fn finish(&self) -> u64 {
                0
            }

            fn write(&mut self, _bytes: &[u8]) {}
        }

        let values = (0..1_000)
            .map(|index| format!("left|{index}|right"))
            .collect::<Vec<_>>();
        let mut keys = HashSet::<SqlKeyRef<'_>, BuildHasherDefault<ConstantHasher>>::default();
        for value in &values {
            assert!(keys.insert(SqlKeyRef::new(ValueRef::String(value))));
            assert!(!keys.insert(SqlKeyRef::new(ValueRef::String(value))));
        }
        assert_eq!(keys.len(), values.len());
    }
}
