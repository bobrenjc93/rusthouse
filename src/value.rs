use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

/// The physical column types supported by RustHouse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataType {
    Int8,
    Int16,
    Int32,
    Int64,
    UInt8,
    UInt16,
    UInt32,
    UInt64,
    Float64,
    Bool,
    String,
}

impl DataType {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_uppercase().as_str() {
            "INT8" => Some(Self::Int8),
            "INT16" => Some(Self::Int16),
            "INT32" => Some(Self::Int32),
            "INT64" => Some(Self::Int64),
            "UINT8" => Some(Self::UInt8),
            "UINT16" => Some(Self::UInt16),
            "UINT32" => Some(Self::UInt32),
            "UINT64" => Some(Self::UInt64),
            "FLOAT64" => Some(Self::Float64),
            "BOOL" | "BOOLEAN" => Some(Self::Bool),
            "STRING" => Some(Self::String),
            _ => None,
        }
    }

    #[must_use]
    pub(crate) fn is_integer(self) -> bool {
        matches!(
            self,
            Self::Int8
                | Self::Int16
                | Self::Int32
                | Self::Int64
                | Self::UInt8
                | Self::UInt16
                | Self::UInt32
                | Self::UInt64
        )
    }

    #[must_use]
    pub(crate) fn is_signed_integer(self) -> bool {
        matches!(self, Self::Int8 | Self::Int16 | Self::Int32 | Self::Int64)
    }

    #[must_use]
    pub(crate) fn is_numeric(self) -> bool {
        self.is_integer() || self == Self::Float64
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Int8 => "Int8",
            Self::Int16 => "Int16",
            Self::Int32 => "Int32",
            Self::Int64 => "Int64",
            Self::UInt8 => "UInt8",
            Self::UInt16 => "UInt16",
            Self::UInt32 => "UInt32",
            Self::UInt64 => "UInt64",
            Self::Float64 => "Float64",
            Self::Bool => "Bool",
            Self::String => "String",
        })
    }
}

/// A scalar value read from or written to a typed column.
#[derive(Debug, Clone)]
pub enum Value {
    Int8(i8),
    Int16(i16),
    Int32(i32),
    Int64(i64),
    UInt8(u8),
    UInt16(u16),
    UInt32(u32),
    UInt64(u64),
    Float64(f64),
    Bool(bool),
    String(String),
}

/// A non-owning scalar used while scanning immutable column storage.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ValueRef<'a> {
    Int8(i8),
    Int16(i16),
    Int32(i32),
    Int64(i64),
    UInt8(u8),
    UInt16(u16),
    UInt32(u32),
    UInt64(u64),
    Float64(f64),
    Bool(bool),
    String(&'a str),
}

impl Value {
    #[must_use]
    pub fn data_type(&self) -> DataType {
        match self {
            Self::Int8(_) => DataType::Int8,
            Self::Int16(_) => DataType::Int16,
            Self::Int32(_) => DataType::Int32,
            Self::Int64(_) => DataType::Int64,
            Self::UInt8(_) => DataType::UInt8,
            Self::UInt16(_) => DataType::UInt16,
            Self::UInt32(_) => DataType::UInt32,
            Self::UInt64(_) => DataType::UInt64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }

    #[must_use]
    pub fn as_display_string(&self) -> String {
        match self {
            Self::Int8(value) => value.to_string(),
            Self::Int16(value) => value.to_string(),
            Self::Int32(value) => value.to_string(),
            Self::Int64(value) => value.to_string(),
            Self::UInt8(value) => value.to_string(),
            Self::UInt16(value) => value.to_string(),
            Self::UInt32(value) => value.to_string(),
            Self::UInt64(value) => value.to_string(),
            Self::Float64(value) => format_float(*value),
            Self::Bool(value) => value.to_string(),
            Self::String(value) => value.clone(),
        }
    }

    pub(crate) fn as_ref(&self) -> ValueRef<'_> {
        match self {
            Self::Int8(value) => ValueRef::Int8(*value),
            Self::Int16(value) => ValueRef::Int16(*value),
            Self::Int32(value) => ValueRef::Int32(*value),
            Self::Int64(value) => ValueRef::Int64(*value),
            Self::UInt8(value) => ValueRef::UInt8(*value),
            Self::UInt16(value) => ValueRef::UInt16(*value),
            Self::UInt32(value) => ValueRef::UInt32(*value),
            Self::UInt64(value) => ValueRef::UInt64(*value),
            Self::Float64(value) => ValueRef::Float64(*value),
            Self::Bool(value) => ValueRef::Bool(*value),
            Self::String(value) => ValueRef::String(value),
        }
    }

    pub(crate) fn checked_coerce_integer(self, target: DataType) -> Option<Self> {
        let value = self.as_ref().integer()?;
        Some(match target {
            DataType::Int8 => Self::Int8(i8::try_from(value).ok()?),
            DataType::Int16 => Self::Int16(i16::try_from(value).ok()?),
            DataType::Int32 => Self::Int32(i32::try_from(value).ok()?),
            DataType::Int64 => Self::Int64(i64::try_from(value).ok()?),
            DataType::UInt8 => Self::UInt8(u8::try_from(value).ok()?),
            DataType::UInt16 => Self::UInt16(u16::try_from(value).ok()?),
            DataType::UInt32 => Self::UInt32(u32::try_from(value).ok()?),
            DataType::UInt64 => Self::UInt64(u64::try_from(value).ok()?),
            DataType::Float64 | DataType::Bool | DataType::String => return None,
        })
    }

    #[cfg(test)]
    pub(crate) fn sql_cmp(&self, other: &Self) -> Option<Ordering> {
        self.as_ref().sql_cmp(other.as_ref())
    }
}

impl ValueRef<'_> {
    pub(crate) fn to_owned(self) -> Value {
        match self {
            Self::Int8(value) => Value::Int8(value),
            Self::Int16(value) => Value::Int16(value),
            Self::Int32(value) => Value::Int32(value),
            Self::Int64(value) => Value::Int64(value),
            Self::UInt8(value) => Value::UInt8(value),
            Self::UInt16(value) => Value::UInt16(value),
            Self::UInt32(value) => Value::UInt32(value),
            Self::UInt64(value) => Value::UInt64(value),
            Self::Float64(value) => Value::Float64(value),
            Self::Bool(value) => Value::Bool(value),
            Self::String(value) => Value::String(value.to_owned()),
        }
    }

    pub(crate) fn sql_cmp(self, other: Self) -> Option<Ordering> {
        if let (Some(left), Some(right)) = (self.integer(), other.integer()) {
            return Some(left.cmp(&right));
        }
        match (self, other) {
            (Self::Float64(left), Self::Float64(right)) => left.partial_cmp(&right),
            (integer, Self::Float64(float)) if integer.integer().is_some() => {
                integer_float_cmp(integer.integer().expect("matched integer"), float)
            }
            (Self::Float64(float), integer) if integer.integer().is_some() => {
                integer_float_cmp(integer.integer().expect("matched integer"), float)
                    .map(Ordering::reverse)
            }
            (Self::Bool(left), Self::Bool(right)) => Some(left.cmp(&right)),
            (Self::String(left), Self::String(right)) => Some(left.cmp(right)),
            _ => None,
        }
    }

    pub(crate) fn integer(self) -> Option<i128> {
        match self {
            Self::Int8(value) => Some(i128::from(value)),
            Self::Int16(value) => Some(i128::from(value)),
            Self::Int32(value) => Some(i128::from(value)),
            Self::Int64(value) => Some(i128::from(value)),
            Self::UInt8(value) => Some(i128::from(value)),
            Self::UInt16(value) => Some(i128::from(value)),
            Self::UInt32(value) => Some(i128::from(value)),
            Self::UInt64(value) => Some(i128::from(value)),
            Self::Float64(_) | Self::Bool(_) | Self::String(_) => None,
        }
    }

    fn variant_index(&self) -> u8 {
        match self {
            Self::Int8(_) => 0,
            Self::Int16(_) => 1,
            Self::Int32(_) => 2,
            Self::Int64(_) => 3,
            Self::UInt8(_) => 4,
            Self::UInt16(_) => 5,
            Self::UInt32(_) => 6,
            Self::UInt64(_) => 7,
            Self::Float64(_) => 8,
            Self::Bool(_) => 9,
            Self::String(_) => 10,
        }
    }
}

fn integer_float_cmp(integer: i128, float: f64) -> Option<Ordering> {
    if float.is_nan() {
        return None;
    }
    let truncated = float.trunc() as i128;
    match integer.cmp(&truncated) {
        Ordering::Equal => (integer as f64).partial_cmp(&float),
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
            (Self::Int8(left), Self::Int8(right)) => left.cmp(right),
            (Self::Int16(left), Self::Int16(right)) => left.cmp(right),
            (Self::Int32(left), Self::Int32(right)) => left.cmp(right),
            (Self::Int64(left), Self::Int64(right)) => left.cmp(right),
            (Self::UInt8(left), Self::UInt8(right)) => left.cmp(right),
            (Self::UInt16(left), Self::UInt16(right)) => left.cmp(right),
            (Self::UInt32(left), Self::UInt32(right)) => left.cmp(right),
            (Self::UInt64(left), Self::UInt64(right)) => left.cmp(right),
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
            Self::Int8(value) => value.hash(state),
            Self::Int16(value) => value.hash(state),
            Self::Int32(value) => value.hash(state),
            Self::Int64(value) => value.hash(state),
            Self::UInt8(value) => value.hash(state),
            Self::UInt16(value) => value.hash(state),
            Self::UInt32(value) => value.hash(state),
            Self::UInt64(value) => value.hash(state),
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
        assert_eq!(
            Value::Int8(-1).sql_cmp(&Value::UInt32(u32::MAX)),
            Some(Ordering::Less)
        );
        assert_eq!(
            Value::UInt64(9_007_199_254_740_993).sql_cmp(&Value::Float64(9_007_199_254_740_992.0)),
            Some(Ordering::Greater)
        );
        assert_eq!(
            Value::UInt64(u64::MAX).sql_cmp(&Value::Float64(18_446_744_073_709_551_616.0)),
            Some(Ordering::Less)
        );
    }

    #[test]
    fn checked_integer_coercion_enforces_signed_and_unsigned_bounds() {
        assert_eq!(
            Value::Int64(-128).checked_coerce_integer(DataType::Int8),
            Some(Value::Int8(-128))
        );
        assert_eq!(
            Value::Int64(255).checked_coerce_integer(DataType::UInt8),
            Some(Value::UInt8(255))
        );
        assert_eq!(
            Value::Int64(256).checked_coerce_integer(DataType::UInt8),
            None
        );
        assert_eq!(
            Value::Int64(-1).checked_coerce_integer(DataType::UInt32),
            None
        );
    }
}
