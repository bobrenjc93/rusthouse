use std::cmp::Ordering;
use std::fmt;

use crate::error::{Error, Result};

/// Scalar types supported by RustHouse columns.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

/// A nullable scalar value used at SQL and result boundaries.
#[derive(Clone, Debug, PartialEq)]
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

    pub(crate) fn coerce(self, target: DataType) -> Result<Self> {
        match (self, target) {
            (Self::Null, _) => Ok(Self::Null),
            (value @ Self::Int64(_), DataType::Int64)
            | (value @ Self::Float64(_), DataType::Float64)
            | (value @ Self::Bool(_), DataType::Bool)
            | (value @ Self::String(_), DataType::String) => Ok(value),
            (Self::Int64(value), DataType::Float64) => Ok(Self::Float64(value as f64)),
            (Self::Int64(0), DataType::Bool) => Ok(Self::Bool(false)),
            (Self::Int64(1), DataType::Bool) => Ok(Self::Bool(true)),
            (value, expected) => Err(Error::Type(format!(
                "cannot store {} in a {expected} column",
                value
                    .data_type()
                    .map_or_else(|| "NULL".to_owned(), |kind| kind.to_string())
            ))),
        }
    }

    pub(crate) fn as_bool(&self, context: &str) -> Result<Option<bool>> {
        match self {
            Self::Null => Ok(None),
            Self::Bool(value) => Ok(Some(*value)),
            _ => Err(Error::Type(format!("{context} requires a Bool expression"))),
        }
    }

    pub(crate) fn sql_cmp(&self, other: &Self) -> Result<Option<Ordering>> {
        match (self, other) {
            (Self::Null, _) | (_, Self::Null) => Ok(None),
            (Self::Int64(left), Self::Int64(right)) => Ok(Some(left.cmp(right))),
            (Self::Float64(left), Self::Float64(right)) => left
                .partial_cmp(right)
                .ok_or_else(|| Error::Type("cannot compare NaN values".to_owned()))
                .map(Some),
            (Self::Int64(left), Self::Float64(right)) => {
                compare_i64_to_f64(*left, *right).map(Some)
            }
            (Self::Float64(left), Self::Int64(right)) => {
                compare_i64_to_f64(*right, *left).map(|ordering| Some(ordering.reverse()))
            }
            (Self::Bool(left), Self::Bool(right)) => Ok(Some(left.cmp(right))),
            (Self::Bool(left), Self::Int64(right)) => Ok(Some((*left as i64).cmp(right))),
            (Self::Int64(left), Self::Bool(right)) => Ok(Some(left.cmp(&(*right as i64)))),
            (Self::String(left), Self::String(right)) => Ok(Some(left.cmp(right))),
            _ => Err(Error::Type(format!(
                "cannot compare {} and {}",
                self.type_name(),
                other.type_name()
            ))),
        }
    }

    pub(crate) fn type_name(&self) -> &'static str {
        match self {
            Self::Null => "NULL",
            Self::Int64(_) => "Int64",
            Self::Float64(_) => "Float64",
            Self::Bool(_) => "Bool",
            Self::String(_) => "String",
        }
    }
}

fn compare_i64_to_f64(integer: i64, float: f64) -> Result<Ordering> {
    if float.is_nan() {
        return Err(Error::Type("cannot compare NaN values".to_owned()));
    }
    if float < i64::MIN as f64 {
        return Ok(Ordering::Greater);
    }
    // i64::MAX rounds up to 2^63 as Float64, so this boundary is exclusive
    // for every Int64 even though a direct `as f64` comparison is not.
    if float >= 9_223_372_036_854_775_808.0 {
        return Ok(Ordering::Less);
    }

    let truncated = float.trunc() as i64;
    let ordering = integer.cmp(&truncated);
    if ordering != Ordering::Equal || float.fract() == 0.0 {
        return Ok(ordering);
    }
    if float.is_sign_positive() {
        Ok(Ordering::Less)
    } else {
        Ok(Ordering::Greater)
    }
}
