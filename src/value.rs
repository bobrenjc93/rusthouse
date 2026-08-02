use std::fmt;

/// The physical data types supported by a RustHouse column.
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

/// A scalar value that can be inserted into or read from a [`crate::Table`].
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// A signed 64-bit integer value.
    Int64(i64),
    /// A double-precision floating-point value.
    Float64(f64),
    /// A boolean value.
    Bool(bool),
    /// An owned UTF-8 string value.
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
