//! Bounded, semicolon-delimited SQL batch execution for the CLI CSV protocol.

pub mod catalog;
pub mod engine;
pub mod error;
pub mod format;
pub mod sql;
pub mod storage;
pub mod value;

use std::error::Error as StdError;
use std::fmt;
use std::io::{self, Read, Write};

use engine::{Database, StatementResult};
use format::{OutputFormat, render};

/// Default maximum SQL batch size accepted from standard input.
///
/// This accommodates the fixed quick and default comparison harness datasets
/// while keeping allocation and parser work bounded by an explicit byte cap.
pub const DEFAULT_MAX_BATCH_BYTES: usize = 64 * 1024 * 1024;

/// A failure while reading, executing, or rendering a CSV SQL batch.
#[derive(Debug)]
pub enum BatchError {
    /// Reading the complete input batch failed.
    Read(io::Error),
    /// The batch crossed the configured input byte limit.
    InputLimitExceeded { bytes: usize, max_bytes: usize },
    /// SQL input was not UTF-8.
    InvalidUtf8(std::string::FromUtf8Error),
    /// Parsing or executing a statement failed.
    Sql(error::Error),
    /// Writing CSV output failed.
    Write(io::Error),
}

impl fmt::Display for BatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(error) => write!(formatter, "could not read SQL from stdin: {error}"),
            Self::InputLimitExceeded { bytes, max_bytes } => write!(
                formatter,
                "SQL batch has at least {bytes} bytes, exceeding the limit of {max_bytes} bytes"
            ),
            Self::InvalidUtf8(error) => write!(formatter, "SQL input is not valid UTF-8: {error}"),
            Self::Sql(error) => error.fmt(formatter),
            Self::Write(error) => write!(formatter, "could not write CSV to stdout: {error}"),
        }
    }
}

impl StdError for BatchError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Read(error) | Self::Write(error) => Some(error),
            Self::InvalidUtf8(error) => Some(error),
            Self::Sql(error) => Some(error),
            Self::InputLimitExceeded { .. } => None,
        }
    }
}

/// Reads one bounded SQL batch to EOF and emits CSVWithNames for every query.
///
/// `CREATE TABLE` and `INSERT` statements are silent. All statements share one
/// in-memory catalog, and the SQL parser handles semicolons inside string
/// literals rather than splitting on raw bytes.
pub fn run_csv_batch(input: impl Read, output: impl Write) -> Result<(), BatchError> {
    run_csv_batch_with_limit(input, output, DEFAULT_MAX_BATCH_BYTES)
}

/// Executes the CSV batch protocol with an explicit input byte limit.
pub fn run_csv_batch_with_limit(
    input: impl Read,
    mut output: impl Write,
    max_input_bytes: usize,
) -> Result<(), BatchError> {
    let read_limit = max_input_bytes.saturating_add(1);
    let mut bytes = Vec::new();
    input
        .take(read_limit as u64)
        .read_to_end(&mut bytes)
        .map_err(BatchError::Read)?;
    if bytes.len() > max_input_bytes {
        return Err(BatchError::InputLimitExceeded {
            bytes: read_limit,
            max_bytes: max_input_bytes,
        });
    }

    let sql = String::from_utf8(bytes).map_err(BatchError::InvalidUtf8)?;
    let mut database = Database::new();
    let results = database.execute(&sql).map_err(BatchError::Sql)?;
    for result in results {
        if let StatementResult::Query(query) = result {
            output
                .write_all(render(&query, OutputFormat::Csv).as_bytes())
                .map_err(BatchError::Write)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_is_silent_and_each_select_has_its_own_header() {
        let input = b"CREATE TABLE t (n Int64, note String);\n\
            INSERT INTO t VALUES (1, 'semi;colon'), (2, 'comma,value');\n\
            SELECT n, note FROM t ORDER BY n;\n\
            SELECT COUNT(*) AS rows, SUM(n) AS total FROM t;\n";
        let mut output = Vec::new();

        run_csv_batch(&input[..], &mut output).expect("batch succeeds");

        assert_eq!(
            output,
            b"n,note\n1,semi;colon\n2,\"comma,value\"\nrows,total\n2,3\n"
        );
    }

    #[test]
    fn explicit_input_limit_is_checked_after_the_last_allowed_byte() {
        let error = run_csv_batch_with_limit(&b"SELECT"[..], Vec::new(), 5)
            .expect_err("six bytes exceed the limit");

        assert!(matches!(
            error,
            BatchError::InputLimitExceeded {
                bytes: 6,
                max_bytes: 5
            }
        ));
    }
}
