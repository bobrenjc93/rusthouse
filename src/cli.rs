//! Bounded stdin batch execution for the `rusthouse` command.

use std::error::Error;
use std::fmt;
use std::io::{self, BufRead};

use crate::{Catalog, CatalogError, ParseErrorKind, TableError};

/// Maximum number of SQL statements accepted in one process invocation.
pub const MAX_BATCH_STATEMENTS: usize = 10_000;
/// Maximum number of bytes accepted for one statement, excluding its line ending.
pub const MAX_STATEMENT_BYTES: usize = 1024 * 1024;
/// Maximum number of stdin bytes accepted in one process invocation.
pub const MAX_BATCH_BYTES: usize = 16 * 1024 * 1024;

/// Exit status used for malformed supported statements and execution failures.
pub const EXIT_EXECUTION_ERROR: u8 = 1;
/// Exit status used for invalid command-line arguments.
pub const EXIT_USAGE_ERROR: u8 = 2;
/// Exit status used when a stdin resource limit is exceeded.
pub const EXIT_LIMIT_ERROR: u8 = 3;
/// Exit status used for SQL statement families the CLI does not execute.
pub const EXIT_UNSUPPORTED_STATEMENT: u8 = 4;
/// Exit status used when stdin cannot be read.
pub const EXIT_INPUT_ERROR: u8 = 5;

/// Counts completed work in a successful stdin batch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BatchSummary {
    /// Number of nonempty statements executed.
    pub statements: usize,
    /// Number of tables created.
    pub tables_created: usize,
    /// Number of rows inserted.
    pub rows_inserted: usize,
}

/// A deterministic failure at the command's stdin boundary.
#[derive(Debug)]
pub enum BatchError {
    /// A physical input line exceeded [`MAX_STATEMENT_BYTES`].
    StatementTooLong {
        /// One-based physical input line.
        line: usize,
        /// Maximum accepted statement length.
        limit: usize,
    },
    /// Total bytes read from stdin exceeded [`MAX_BATCH_BYTES`].
    BatchTooLarge {
        /// One-based physical input line where the limit was crossed.
        line: usize,
        /// Maximum accepted stdin length.
        limit: usize,
    },
    /// Nonempty input lines exceeded [`MAX_BATCH_STATEMENTS`].
    TooManyStatements {
        /// One-based physical input line containing the excess statement.
        line: usize,
        /// Maximum accepted number of statements.
        limit: usize,
    },
    /// A parser, catalog, or table capacity bound was exceeded.
    ExecutionLimit {
        /// One-based physical input line.
        line: usize,
        /// Typed limit failure from the catalog.
        source: CatalogError,
    },
    /// A physical input line was not valid UTF-8.
    InvalidUtf8 {
        /// One-based physical input line.
        line: usize,
    },
    /// The statement does not begin with `CREATE` or `INSERT`.
    UnsupportedStatement {
        /// One-based physical input line.
        line: usize,
    },
    /// A supported statement could not be parsed or executed.
    Execution {
        /// One-based physical input line.
        line: usize,
        /// Typed parse or execution failure from the catalog.
        source: CatalogError,
    },
    /// Reading stdin failed.
    InputRead {
        /// One-based physical input line being read.
        line: usize,
        /// Underlying input failure.
        source: io::Error,
    },
}

impl BatchError {
    /// Returns the stable process exit status for this failure category.
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::StatementTooLong { .. }
            | Self::BatchTooLarge { .. }
            | Self::TooManyStatements { .. }
            | Self::ExecutionLimit { .. } => EXIT_LIMIT_ERROR,
            Self::UnsupportedStatement { .. } => EXIT_UNSUPPORTED_STATEMENT,
            Self::InputRead { .. } => EXIT_INPUT_ERROR,
            Self::InvalidUtf8 { .. } | Self::Execution { .. } => EXIT_EXECUTION_ERROR,
        }
    }
}

impl fmt::Display for BatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StatementTooLong { line, limit } => write!(
                formatter,
                "input limit exceeded on line {line}: statement exceeds {limit} bytes"
            ),
            Self::BatchTooLarge { line, limit } => write!(
                formatter,
                "input limit exceeded on line {line}: stdin exceeds {limit} bytes"
            ),
            Self::TooManyStatements { line, limit } => write!(
                formatter,
                "input limit exceeded on line {line}: batch exceeds {limit} statements"
            ),
            Self::ExecutionLimit { line, source } => {
                write!(
                    formatter,
                    "resource limit exceeded on line {line}: {source}"
                )
            }
            Self::InvalidUtf8 { line } => {
                write!(
                    formatter,
                    "input error on line {line}: statement is not valid UTF-8"
                )
            }
            Self::UnsupportedStatement { line } => write!(
                formatter,
                "unsupported statement on line {line}: expected CREATE TABLE or INSERT INTO"
            ),
            Self::Execution { line, source } => {
                write!(formatter, "execution error on line {line}: {source}")
            }
            Self::InputRead { line, .. } => {
                write!(
                    formatter,
                    "input error on line {line}: could not read stdin"
                )
            }
        }
    }
}

impl Error for BatchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ExecutionLimit { source, .. } | Self::Execution { source, .. } => Some(source),
            Self::InputRead { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Executes one SQL statement per nonempty input line in a shared [`Catalog`].
///
/// Only current `CREATE TABLE` and `INSERT INTO ... VALUES` syntax is dispatched.
/// Processing stops at the first error; statements completed before it remain in
/// `catalog`. The input buffer never grows beyond one bounded physical line.
pub fn execute_batch<R: BufRead>(
    mut input: R,
    catalog: &mut Catalog,
) -> Result<BatchSummary, BatchError> {
    let mut summary = BatchSummary::default();
    let mut total_bytes = 0;
    let mut line_number = 0;
    let mut line = Vec::new();

    loop {
        line_number += 1;
        let remaining_batch_bytes = MAX_BATCH_BYTES - total_bytes;
        let bytes_read =
            read_bounded_line(&mut input, &mut line, remaining_batch_bytes).map_err(|source| {
                BatchError::InputRead {
                    line: line_number,
                    source,
                }
            })?;
        if bytes_read == 0 {
            break;
        }

        total_bytes += bytes_read;
        if total_bytes > MAX_BATCH_BYTES {
            return Err(BatchError::BatchTooLarge {
                line: line_number,
                limit: MAX_BATCH_BYTES,
            });
        }

        strip_line_ending(&mut line);
        if line.len() > MAX_STATEMENT_BYTES {
            return Err(BatchError::StatementTooLong {
                line: line_number,
                limit: MAX_STATEMENT_BYTES,
            });
        }

        let statement = std::str::from_utf8(&line)
            .map_err(|_| BatchError::InvalidUtf8 { line: line_number })?;
        let Some(kind) = statement_kind(statement) else {
            continue;
        };

        if summary.statements == MAX_BATCH_STATEMENTS {
            return Err(BatchError::TooManyStatements {
                line: line_number,
                limit: MAX_BATCH_STATEMENTS,
            });
        }

        match kind {
            StatementKind::Create => {
                catalog
                    .execute_create(statement)
                    .map_err(|source| execution_error(line_number, source))?;
                summary.tables_created += 1;
            }
            StatementKind::Insert => {
                let inserted = catalog
                    .execute_insert(statement)
                    .map_err(|source| execution_error(line_number, source))?;
                summary.rows_inserted += inserted;
            }
            StatementKind::Unsupported => {
                return Err(BatchError::UnsupportedStatement { line: line_number });
            }
        }
        summary.statements += 1;
    }

    Ok(summary)
}

fn execution_error(line: usize, source: CatalogError) -> BatchError {
    if is_catalog_limit(&source) {
        BatchError::ExecutionLimit { line, source }
    } else {
        BatchError::Execution { line, source }
    }
}

fn is_catalog_limit(error: &CatalogError) -> bool {
    match error {
        CatalogError::Parse(error) => matches!(
            error.kind,
            ParseErrorKind::InputTooLong { .. }
                | ParseErrorKind::TooManyColumns { .. }
                | ParseErrorKind::TooManyRows { .. }
                | ParseErrorKind::TooManyValues { .. }
                | ParseErrorKind::TooManyProjections { .. }
                | ParseErrorKind::StringTooLong { .. }
        ),
        CatalogError::TableInsertion {
            source: TableError::RowLimitExceeded { .. },
            ..
        }
        | CatalogError::TableLimitExceeded { .. } => true,
        _ => false,
    }
}

fn read_bounded_line<R: BufRead>(
    input: &mut R,
    line: &mut Vec<u8>,
    remaining_batch_bytes: usize,
) -> io::Result<usize> {
    line.clear();
    let statement_read_limit = MAX_STATEMENT_BYTES + 2;
    let batch_read_limit = remaining_batch_bytes.saturating_add(1);
    let read_limit = statement_read_limit.min(batch_read_limit) as u64;
    std::io::Read::take(input, read_limit).read_until(b'\n', line)
}

fn strip_line_ending(line: &mut Vec<u8>) {
    if line.last() == Some(&b'\n') {
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatementKind {
    Create,
    Insert,
    Unsupported,
}

fn statement_kind(statement: &str) -> Option<StatementKind> {
    let statement = statement.as_bytes();
    let start = statement
        .iter()
        .position(|byte| !is_sql_whitespace(*byte))?;
    let end = statement[start..]
        .iter()
        .position(|byte| is_token_separator(*byte))
        .map_or(statement.len(), |length| start + length);
    let keyword = &statement[start..end];

    if keyword.eq_ignore_ascii_case(b"CREATE") {
        Some(StatementKind::Create)
    } else if keyword.eq_ignore_ascii_case(b"INSERT") {
        Some(StatementKind::Insert)
    } else {
        Some(StatementKind::Unsupported)
    }
}

const fn is_token_separator(byte: u8) -> bool {
    is_sql_whitespace(byte) || matches!(byte, b'(' | b')' | b',' | b';' | b'*')
}

const fn is_sql_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}
