use super::ParseError;

/// Maximum SQL statement size accepted by the SQL parsers, in bytes.
pub const MAX_SQL_BYTES: usize = 64 * 1024;

/// Maximum number of columns accepted by [`crate::sql::parse_create_table`].
pub const MAX_COLUMNS: usize = 1_024;

/// Maximum number of rows accepted by [`crate::sql::parse_insert`].
pub const MAX_INSERT_ROWS: usize = 10_000;

/// Maximum total number of values accepted by [`crate::sql::parse_insert`].
pub const MAX_INSERT_VALUES: usize = 16_384;

/// Maximum decoded String payload accepted by [`crate::sql::parse_insert`], in bytes.
pub const MAX_INSERT_STRING_BYTES: usize = 32 * 1024;

/// Resource limits applied while parsing a SQL statement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParseLimits {
    /// Maximum accepted statement length, in bytes.
    pub max_sql_bytes: usize,
    /// Maximum number of column definitions in one statement.
    pub max_columns: usize,
}

impl Default for ParseLimits {
    fn default() -> Self {
        Self {
            max_sql_bytes: MAX_SQL_BYTES,
            max_columns: MAX_COLUMNS,
        }
    }
}

/// Resource limits applied while parsing an INSERT statement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InsertParseLimits {
    /// Maximum accepted statement length, in bytes.
    pub max_sql_bytes: usize,
    /// Maximum number of rows in one statement.
    pub max_rows: usize,
    /// Maximum total number of values across all rows in one statement.
    pub max_values: usize,
    /// Maximum decoded UTF-8 bytes across all String literals.
    pub max_string_bytes: usize,
}

impl Default for InsertParseLimits {
    fn default() -> Self {
        Self {
            max_sql_bytes: MAX_SQL_BYTES,
            max_rows: MAX_INSERT_ROWS,
            max_values: MAX_INSERT_VALUES,
            max_string_bytes: MAX_INSERT_STRING_BYTES,
        }
    }
}

/// Resource limits applied while parsing a SELECT statement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectParseLimits {
    /// Maximum accepted statement length, in bytes.
    pub max_sql_bytes: usize,
}

impl Default for SelectParseLimits {
    fn default() -> Self {
        Self {
            max_sql_bytes: MAX_SQL_BYTES,
        }
    }
}

pub(super) fn enforce_sql_size(sql: &str, max_bytes: usize) -> Result<(), ParseError> {
    if sql.len() > max_bytes {
        return Err(ParseError::SqlTooLarge {
            position: max_bytes,
            max_bytes,
            actual_bytes: sql.len(),
        });
    }

    Ok(())
}
