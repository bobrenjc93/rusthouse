use std::cmp::Ordering;
use std::fmt;

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    Int64,
    Float64,
    Bool,
    String,
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Int64 => f.write_str("Int64"),
            Self::Float64 => f.write_str("Float64"),
            Self::Bool => f.write_str("Bool"),
            Self::String => f.write_str("String"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int64(i64),
    Float64(f64),
    Bool(bool),
    String(String),
}

impl Value {
    pub fn data_type(&self) -> Option<DataType> {
        match self {
            Self::Null => None,
            Self::Int64(_) => Some(DataType::Int64),
            Self::Float64(_) => Some(DataType::Float64),
            Self::Bool(_) => Some(DataType::Bool),
            Self::String(_) => Some(DataType::String),
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    pub fn sql_bool(&self) -> Result<Option<bool>> {
        match self {
            Self::Null => Ok(None),
            Self::Bool(value) => Ok(Some(*value)),
            value => Err(Error::Type(format!(
                "expected Bool, found {}",
                value.type_name()
            ))),
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Null => "NULL",
            Self::Int64(_) => "Int64",
            Self::Float64(_) => "Float64",
            Self::Bool(_) => "Bool",
            Self::String(_) => "String",
        }
    }

    pub(crate) fn total_cmp(&self, other: &Self) -> Result<Ordering> {
        match (self, other) {
            (Self::Null, Self::Null) => Ok(Ordering::Equal),
            (Self::Null, _) | (_, Self::Null) => Err(Error::Type(
                "NULL ordering must be handled by the caller".into(),
            )),
            (Self::Int64(left), Self::Int64(right)) => Ok(left.cmp(right)),
            (Self::Float64(left), Self::Float64(right)) => Ok(left.total_cmp(right)),
            (Self::Int64(left), Self::Float64(right)) => Ok((*left as f64).total_cmp(right)),
            (Self::Float64(left), Self::Int64(right)) => Ok(left.total_cmp(&(*right as f64))),
            (Self::Bool(left), Self::Bool(right)) => Ok(left.cmp(right)),
            (Self::String(left), Self::String(right)) => Ok(left.cmp(right)),
            (left, right) => Err(Error::Type(format!(
                "cannot compare {} with {}",
                left.type_name(),
                right.type_name()
            ))),
        }
    }

    pub(crate) fn sql_cmp(&self, other: &Self) -> Result<Ordering> {
        match (self, other) {
            (Self::Int64(left), Self::Int64(right)) => Ok(left.cmp(right)),
            (Self::Float64(left), Self::Float64(right)) => left
                .partial_cmp(right)
                .ok_or_else(|| Error::Type("NaN cannot be compared".into())),
            (Self::Int64(left), Self::Float64(right)) => (*left as f64)
                .partial_cmp(right)
                .ok_or_else(|| Error::Type("NaN cannot be compared".into())),
            (Self::Float64(left), Self::Int64(right)) => left
                .partial_cmp(&(*right as f64))
                .ok_or_else(|| Error::Type("NaN cannot be compared".into())),
            (Self::Bool(left), Self::Bool(right)) => Ok(left.cmp(right)),
            (Self::String(left), Self::String(right)) => Ok(left.cmp(right)),
            (left, right) => Err(Error::Type(format!(
                "cannot compare {} with {}",
                left.type_name(),
                right.type_name()
            ))),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("NULL"),
            Self::Int64(value) => write!(f, "{value}"),
            Self::Float64(value) => write!(f, "{value}"),
            Self::Bool(value) => write!(f, "{value}"),
            Self::String(value) => f.write_str(value),
        }
    }
}
