//! Scalar types and values supported by the storage layer.

use std::fmt;

/// A scalar type supported by RustHouse columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DataType {
    /// A signed 64-bit integer.
    Int64,
    /// An IEEE 754 double-precision floating-point number.
    Float64,
    /// A boolean value.
    Bool,
    /// A UTF-8 string.
    String,
}

impl fmt::Display for DataType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Int64 => "Int64",
            Self::Float64 => "Float64",
            Self::Bool => "Bool",
            Self::String => "String",
        })
    }
}

/// A single, non-null value stored in a table.
#[derive(Clone, Debug, PartialEq)]
pub enum ScalarValue {
    /// A signed 64-bit integer value.
    Int64(i64),
    /// An IEEE 754 double-precision floating-point value.
    Float64(f64),
    /// A boolean value.
    Bool(bool),
    /// A UTF-8 string value.
    String(String),
}

impl ScalarValue {
    /// Returns the logical type of this value.
    #[must_use]
    pub const fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }
}

/// A concise alias for [`ScalarValue`].
pub use ScalarValue as Scalar;

impl From<i64> for ScalarValue {
    fn from(value: i64) -> Self {
        Self::Int64(value)
    }
}

impl From<f64> for ScalarValue {
    fn from(value: f64) -> Self {
        Self::Float64(value)
    }
}

impl From<bool> for ScalarValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<String> for ScalarValue {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for ScalarValue {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}

impl From<&ScalarValue> for ScalarValue {
    fn from(value: &ScalarValue) -> Self {
        value.clone()
    }
}
