use std::{fmt, str::FromStr};

use crate::{Error, Result};

/// Scalar types supported by the expression engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataType {
    Int64,
    Float64,
    Bool,
    String,
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

impl FromStr for DataType {
    type Err = ();

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value.to_ascii_uppercase().as_str() {
            "INT" | "INTEGER" | "BIGINT" | "INT64" => Ok(Self::Int64),
            "FLOAT" | "DOUBLE" | "REAL" | "FLOAT64" => Ok(Self::Float64),
            "BOOL" | "BOOLEAN" => Ok(Self::Bool),
            "STRING" | "TEXT" | "VARCHAR" => Ok(Self::String),
            _ => Err(()),
        }
    }
}

/// A dynamically typed SQL scalar. `Null` has no concrete runtime type and
/// acquires one from the surrounding schema when values are stored.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int64(i64),
    Float64(f64),
    Bool(bool),
    String(String),
}

impl Value {
    pub const fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    pub const fn data_type(&self) -> Option<DataType> {
        match self {
            Self::Null => None,
            Self::Int64(_) => Some(DataType::Int64),
            Self::Float64(_) => Some(DataType::Float64),
            Self::Bool(_) => Some(DataType::Bool),
            Self::String(_) => Some(DataType::String),
        }
    }

    pub const fn type_name(&self) -> &'static str {
        match self {
            Self::Null => "NULL",
            Self::Int64(_) => "Int64",
            Self::Float64(_) => "Float64",
            Self::Bool(_) => "Bool",
            Self::String(_) => "String",
        }
    }

    /// Applies SQL CAST rules. NULL remains NULL, integer conversion truncates
    /// finite floats toward zero, and malformed or out-of-range values fail.
    pub fn cast_to(&self, target: DataType) -> Result<Self> {
        if self.is_null() {
            return Ok(Self::Null);
        }

        let invalid = || Error::InvalidCast {
            value: self.to_string(),
            target,
        };

        match (self, target) {
            (Self::Int64(value), DataType::Int64) => Ok(Self::Int64(*value)),
            (Self::Int64(value), DataType::Float64) => Ok(Self::Float64(*value as f64)),
            (Self::Int64(value), DataType::Bool) => Ok(Self::Bool(*value != 0)),
            (Self::Int64(value), DataType::String) => Ok(Self::String(value.to_string())),

            (Self::Float64(value), DataType::Int64)
                if value.is_finite()
                    && value.trunc() >= i64::MIN as f64
                    && value.trunc() < -(i64::MIN as f64) =>
            {
                Ok(Self::Int64(value.trunc() as i64))
            }
            (Self::Float64(_), DataType::Int64) => Err(invalid()),
            (Self::Float64(value), DataType::Float64) => Ok(Self::Float64(*value)),
            (Self::Float64(value), DataType::Bool) => Ok(Self::Bool(*value != 0.0)),
            (Self::Float64(value), DataType::String) => Ok(Self::String(format_float(*value))),

            (Self::Bool(value), DataType::Int64) => Ok(Self::Int64(i64::from(*value))),
            (Self::Bool(value), DataType::Float64) => {
                Ok(Self::Float64(if *value { 1.0 } else { 0.0 }))
            }
            (Self::Bool(value), DataType::Bool) => Ok(Self::Bool(*value)),
            (Self::Bool(value), DataType::String) => Ok(Self::String(value.to_string())),

            (Self::String(value), DataType::Int64) => value
                .trim()
                .parse::<i64>()
                .map(Self::Int64)
                .map_err(|_| invalid()),
            (Self::String(value), DataType::Float64) => value
                .trim()
                .parse::<f64>()
                .map(Self::Float64)
                .map_err(|_| invalid()),
            (Self::String(value), DataType::Bool) => {
                match value.trim().to_ascii_lowercase().as_str() {
                    "true" | "t" | "1" => Ok(Self::Bool(true)),
                    "false" | "f" | "0" => Ok(Self::Bool(false)),
                    _ => Err(invalid()),
                }
            }
            (Self::String(value), DataType::String) => Ok(Self::String(value.clone())),
            (Self::Null, _) => unreachable!(),
        }
    }
}

fn format_float(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_owned()
    } else if value == f64::INFINITY {
        "Infinity".to_owned()
    } else if value == f64::NEG_INFINITY {
        "-Infinity".to_owned()
    } else {
        value.to_string()
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("NULL"),
            Self::Int64(value) => write!(f, "{value}"),
            Self::Float64(value) => f.write_str(&format_float(*value)),
            Self::Bool(value) => write!(f, "{value}"),
            Self::String(value) => write!(f, "'{value}'"),
        }
    }
}

impl From<i64> for Value {
    fn from(value: i64) -> Self {
        Self::Int64(value)
    }
}

impl From<f64> for Value {
    fn from(value: f64) -> Self {
        Self::Float64(value)
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}
