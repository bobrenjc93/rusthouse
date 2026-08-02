use std::error::Error;
use std::fmt;

/// Error returned while parsing or evaluating a query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryError {
    /// The query contained no statement.
    EmptyQuery,
    /// The query did not match the supported SQL grammar.
    InvalidSyntax {
        /// Zero-based byte offset at which parsing failed.
        position: usize,
        /// Description of what was expected or invalid.
        message: String,
    },
    /// An integer literal did not fit in a signed 64-bit value.
    IntegerOutOfRange {
        /// The rejected SQL token.
        literal: String,
    },
    /// A float literal was not representable as a finite 64-bit value.
    NonFiniteFloat {
        /// The rejected SQL token.
        literal: String,
    },
}

impl QueryError {
    pub(crate) fn syntax(position: usize, message: impl Into<String>) -> Self {
        Self::InvalidSyntax {
            position,
            message: message.into(),
        }
    }
}

impl fmt::Display for QueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyQuery => formatter.write_str("query is empty"),
            Self::InvalidSyntax { position, message } => {
                write!(formatter, "invalid SQL at byte {position}: {message}")
            }
            Self::IntegerOutOfRange { literal } => {
                write!(
                    formatter,
                    "integer literal is outside the Int64 range: {literal}"
                )
            }
            Self::NonFiniteFloat { literal } => {
                write!(formatter, "float literal is not finite: {literal}")
            }
        }
    }
}

impl Error for QueryError {}
