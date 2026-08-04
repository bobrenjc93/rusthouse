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
use format::write_csv;

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
/// `CREATE TABLE` and `INSERT` statements are silent. `SELECT` and `SHOW TABLES`
/// produce query results. All statements share one in-memory catalog, and the
/// SQL parser handles semicolons inside string literals rather than splitting
/// on raw bytes.
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
    let statements = sql::parse(&sql).map_err(BatchError::Sql)?;
    let mut database = Database::new();
    for statement in statements {
        let result = database
            .execute_statement(statement)
            .map_err(BatchError::Sql)?;
        if let StatementResult::Query(query) = result {
            write_csv(&mut output, &query).map_err(BatchError::Write)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailAfterBytes {
        remaining: usize,
        written: usize,
    }

    impl Write for FailAfterBytes {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::Error::other("intentional writer stop"));
            }
            let written = self.remaining.min(buffer.len());
            self.remaining -= written;
            self.written += written;
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

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

    #[test]
    fn streams_large_repeated_results_before_executing_later_statements() {
        let large_value = "x".repeat(1024 * 1024);
        let mut sql =
            format!("CREATE TABLE notes (s String); INSERT INTO notes VALUES ('{large_value}');");
        for _ in 0..256 {
            sql.push_str("SELECT s FROM notes;");
        }
        sql.push_str("SELECT missing FROM notes;");
        let mut output = FailAfterBytes {
            remaining: 128,
            written: 0,
        };

        let error = run_csv_batch(sql.as_bytes(), &mut output)
            .expect_err("writer stops during the first large result");

        assert!(matches!(error, BatchError::Write(_)));
        assert_eq!(output.written, 128);
    }
}
