use std::fmt;

use crate::DataType;

/// Errors produced while parsing or evaluating SQL scalar expressions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Parse {
        message: String,
        position: usize,
    },
    UnknownColumn(String),
    Type {
        operation: String,
        expected: String,
        actual: String,
    },
    Overflow {
        operation: String,
    },
    DivideByZero,
    InvalidCast {
        value: String,
        target: DataType,
    },
    InvalidArgument {
        function: String,
        message: String,
    },
    Aggregate(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse { message, position } => {
                write!(f, "SQL parse error at byte {position}: {message}")
            }
            Self::UnknownColumn(name) => write!(f, "unknown column `{name}`"),
            Self::Type {
                operation,
                expected,
                actual,
            } => write!(
                f,
                "type error in {operation}: expected {expected}, got {actual}"
            ),
            Self::Overflow { operation } => write!(f, "numeric overflow in {operation}"),
            Self::DivideByZero => f.write_str("division by zero"),
            Self::InvalidCast { value, target } => {
                write!(f, "cannot cast {value} to {target}")
            }
            Self::InvalidArgument { function, message } => {
                write!(f, "invalid argument to {function}: {message}")
            }
            Self::Aggregate(message) => write!(f, "aggregate error: {message}"),
        }
    }
}

impl std::error::Error for Error {}
