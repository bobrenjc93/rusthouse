use std::error::Error;
use std::fmt;

/// A typed SQL parse failure.
///
/// Positions are zero-based byte offsets into the original SQL string. At end
/// of input, the position equals the string's byte length.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParseError {
    /// The input did not match the supported grammar.
    Syntax {
        /// Zero-based byte offset at which parsing failed.
        position: usize,
        /// Description of the grammar element required at `position`.
        expected: &'static str,
        /// Token found at `position`, or `None` at the end of input.
        found: Option<String>,
    },
    /// A syntactically valid type name is not supported.
    UnsupportedType {
        /// Zero-based byte offset of the type name.
        position: usize,
        /// Unsupported type name exactly as it appeared in the statement.
        type_name: String,
    },
    /// Non-whitespace input followed the statement or its optional semicolon.
    TrailingInput {
        /// Zero-based byte offset of the first trailing token.
        position: usize,
    },
    /// The input exceeded the configured byte limit.
    SqlTooLarge {
        /// Byte offset equal to the first byte beyond the configured limit.
        position: usize,
        /// Configured maximum statement length, in bytes.
        max_bytes: usize,
        /// Actual statement length, in bytes.
        actual_bytes: usize,
    },
    /// The statement exceeded the configured column limit.
    TooManyColumns {
        /// Zero-based byte offset of the first excess column.
        position: usize,
        /// Configured maximum number of columns.
        max_columns: usize,
    },
    /// An INSERT exceeded the configured row limit.
    TooManyRows {
        /// Zero-based byte offset of the first excess row.
        position: usize,
        /// Configured maximum number of rows.
        max_rows: usize,
    },
    /// An INSERT exceeded the configured total value limit.
    TooManyValues {
        /// Zero-based byte offset of the first excess value.
        position: usize,
        /// Configured maximum number of values.
        max_values: usize,
    },
    /// String literals exceeded the configured decoded UTF-8 byte limit.
    StringByteLimitExceeded {
        /// Zero-based byte offset of the String literal that exceeded the limit.
        position: usize,
        /// Configured maximum decoded String payload, in bytes.
        max_bytes: usize,
        /// Decoded String payload that the statement attempted, in bytes.
        attempted_bytes: usize,
    },
    /// An integer literal was outside the `i64` range.
    IntegerOverflow {
        /// Zero-based byte offset of the integer literal.
        position: usize,
        /// Out-of-range literal exactly as it appeared in the statement.
        literal: String,
    },
    /// A floating-point literal evaluated to a non-finite value.
    NonFiniteFloat {
        /// Zero-based byte offset of the floating-point literal.
        position: usize,
        /// Non-finite literal exactly as it appeared in the statement.
        literal: String,
    },
}

impl ParseError {
    /// Returns the zero-based byte offset associated with this error.
    pub const fn position(&self) -> usize {
        match self {
            Self::Syntax { position, .. }
            | Self::UnsupportedType { position, .. }
            | Self::TrailingInput { position }
            | Self::SqlTooLarge { position, .. }
            | Self::TooManyColumns { position, .. }
            | Self::TooManyRows { position, .. }
            | Self::TooManyValues { position, .. }
            | Self::StringByteLimitExceeded { position, .. }
            | Self::IntegerOverflow { position, .. }
            | Self::NonFiniteFloat { position, .. } => *position,
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax {
                position,
                expected,
                found,
            } => match found {
                Some(found) => write!(
                    formatter,
                    "expected {expected} at byte {position}, found {found:?}"
                ),
                None => write!(
                    formatter,
                    "expected {expected} at byte {position}, found end of input"
                ),
            },
            Self::UnsupportedType {
                position,
                type_name,
            } => write!(
                formatter,
                "unsupported type {type_name:?} at byte {position}"
            ),
            Self::TrailingInput { position } => {
                write!(formatter, "trailing input at byte {position}")
            }
            Self::SqlTooLarge {
                position,
                max_bytes,
                actual_bytes,
            } => write!(
                formatter,
                "SQL is {actual_bytes} bytes, exceeding the {max_bytes}-byte limit at byte {position}"
            ),
            Self::TooManyColumns {
                position,
                max_columns,
            } => write!(
                formatter,
                "column at byte {position} exceeds the {max_columns}-column limit"
            ),
            Self::TooManyRows { position, max_rows } => write!(
                formatter,
                "row at byte {position} exceeds the {max_rows}-row limit"
            ),
            Self::TooManyValues {
                position,
                max_values,
            } => write!(
                formatter,
                "value at byte {position} exceeds the {max_values}-value limit"
            ),
            Self::StringByteLimitExceeded {
                position,
                max_bytes,
                attempted_bytes,
            } => write!(
                formatter,
                "String literals total {attempted_bytes} bytes, exceeding the {max_bytes}-byte limit at byte {position}"
            ),
            Self::IntegerOverflow { position, literal } => write!(
                formatter,
                "integer literal {literal:?} overflows Int64 at byte {position}"
            ),
            Self::NonFiniteFloat { position, literal } => write!(
                formatter,
                "floating-point literal {literal:?} is non-finite at byte {position}"
            ),
        }
    }
}

impl Error for ParseError {}
