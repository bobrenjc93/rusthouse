use std::fmt;

/// A physical data type supported by RustHouse columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataType {
    /// A signed 64-bit integer.
    Int64,
    /// A finite IEEE 754 double-precision number.
    Float64,
    /// A boolean value.
    Bool,
    /// An owned UTF-8 string.
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

/// A scalar value that can be inserted into or read from a [`crate::Column`].
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A signed 64-bit integer.
    Int64(i64),
    /// An IEEE 754 double-precision number.
    Float64(f64),
    /// A boolean value.
    Bool(bool),
    /// An owned UTF-8 string.
    String(String),
}

impl Value {
    /// Returns the physical type of this value.
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

impl fmt::Display for Value {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Int64(value) => write!(formatter, "{value}"),
            Self::Float64(value) => write!(formatter, "{value}"),
            Self::Bool(value) => write!(formatter, "{value}"),
            Self::String(value) => formatter.write_str(value),
        }
    }
}
