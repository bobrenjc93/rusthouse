//! Homogeneously typed, in-memory columns.

use std::error::Error;
use std::fmt;

use crate::scalar::{DataType, ScalarValue};

/// A column backed by a vector matching its logical type.
#[derive(Clone, Debug, PartialEq)]
pub enum Column {
    /// Signed 64-bit integer values.
    Int64(Vec<i64>),
    /// Double-precision floating-point values.
    Float64(Vec<f64>),
    /// Boolean values.
    Bool(Vec<bool>),
    /// UTF-8 string values.
    String(Vec<String>),
}

impl Column {
    /// Creates an empty column of `data_type`.
    #[must_use]
    pub const fn new(data_type: DataType) -> Self {
        match data_type {
            DataType::Int64 => Self::Int64(Vec::new()),
            DataType::Float64 => Self::Float64(Vec::new()),
            DataType::Bool => Self::Bool(Vec::new()),
            DataType::String => Self::String(Vec::new()),
        }
    }

    /// Returns the column's logical type.
    #[must_use]
    pub const fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }

    /// Returns the number of stored values.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Int64(values) => values.len(),
            Self::Float64(values) => values.len(),
            Self::Bool(values) => values.len(),
            Self::String(values) => values.len(),
        }
    }

    /// Returns whether the column contains no values.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Appends a value if its type matches this column.
    ///
    /// A type mismatch leaves the column unchanged.
    pub fn push(&mut self, value: ScalarValue) -> Result<(), ColumnError> {
        let actual = value.data_type();
        let expected = self.data_type();

        match (self, value) {
            (Self::Int64(values), ScalarValue::Int64(value)) => values.push(value),
            (Self::Float64(values), ScalarValue::Float64(value)) => values.push(value),
            (Self::Bool(values), ScalarValue::Bool(value)) => values.push(value),
            (Self::String(values), ScalarValue::String(value)) => values.push(value),
            _ => return Err(ColumnError::TypeMismatch { expected, actual }),
        }

        Ok(())
    }

    /// Returns the value at `index`, cloning strings when needed.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<ScalarValue> {
        match self {
            Self::Int64(values) => values.get(index).copied().map(ScalarValue::Int64),
            Self::Float64(values) => values.get(index).copied().map(ScalarValue::Float64),
            Self::Bool(values) => values.get(index).copied().map(ScalarValue::Bool),
            Self::String(values) => values.get(index).cloned().map(ScalarValue::String),
        }
    }

    /// Returns this column as an `Int64` slice, if its type matches.
    #[must_use]
    pub fn as_int64(&self) -> Option<&[i64]> {
        match self {
            Self::Int64(values) => Some(values),
            _ => None,
        }
    }

    /// Returns this column as a `Float64` slice, if its type matches.
    #[must_use]
    pub fn as_float64(&self) -> Option<&[f64]> {
        match self {
            Self::Float64(values) => Some(values),
            _ => None,
        }
    }

    /// Returns this column as a `Bool` slice, if its type matches.
    #[must_use]
    pub fn as_bool(&self) -> Option<&[bool]> {
        match self {
            Self::Bool(values) => Some(values),
            _ => None,
        }
    }

    /// Returns this column as a `String` slice, if its type matches.
    #[must_use]
    pub fn as_string(&self) -> Option<&[String]> {
        match self {
            Self::String(values) => Some(values),
            _ => None,
        }
    }
}

/// A column mutation failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnError {
    /// The value's type did not match the column type.
    TypeMismatch {
        expected: DataType,
        actual: DataType,
    },
}

impl fmt::Display for ColumnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TypeMismatch { expected, actual } => {
                write!(formatter, "expected {expected}, received {actual}")
            }
        }
    }
}

impl Error for ColumnError {}
