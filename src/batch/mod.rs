//! Bounded, semicolon-delimited SQL batch execution for CLI export formats.

pub mod catalog;
pub mod csv;
pub mod engine;
pub mod error;
pub mod format;
pub mod shared_database;
pub mod sql;
pub mod storage;
pub mod value;

pub use shared_database::{SharedDatabase, SharedDatabaseError};

use std::error::Error as StdError;
use std::fmt;
use std::io::{self, Read, Write};

use engine::{DEFAULT_MAX_QUERY_RESULT_BYTES, Database, StatementResult};
use format::{TableWriteError, write_csv, write_json, write_table_with_affixes};

/// Default maximum SQL batch size accepted from standard input.
///
/// This accommodates the fixed quick and default comparison harness datasets
/// while keeping allocation and parser work bounded by an explicit byte cap.
pub const DEFAULT_MAX_BATCH_BYTES: usize = 64 * 1024 * 1024;

/// Maximum bytes emitted for one formatted table result, including separators.
pub const DEFAULT_MAX_TABLE_OUTPUT_BYTES: usize = DEFAULT_MAX_QUERY_RESULT_BYTES;

/// A failure while reading, executing, or rendering a SQL batch.
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
    /// A padded table result crossed the formatted-output byte limit.
    TableOutputLimitExceeded { bytes: usize, max_bytes: usize },
    /// Writing human-readable table output failed.
    WriteTable(io::Error),
    /// Writing CSV output failed.
    Write(io::Error),
    /// Writing newline-delimited JSON output failed.
    WriteJson(io::Error),
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
            Self::TableOutputLimitExceeded { bytes, max_bytes } => write!(
                formatter,
                "table output requires at least {bytes} bytes, exceeding the limit of {max_bytes} bytes"
            ),
            Self::WriteTable(error) => {
                write!(formatter, "could not write table to stdout: {error}")
            }
            Self::Write(error) => write!(formatter, "could not write CSV to stdout: {error}"),
            Self::WriteJson(error) => write!(formatter, "could not write JSON to stdout: {error}"),
        }
    }
}

impl StdError for BatchError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Read(error)
            | Self::WriteTable(error)
            | Self::Write(error)
            | Self::WriteJson(error) => Some(error),
            Self::InvalidUtf8(error) => Some(error),
            Self::Sql(error) => Some(error),
            Self::InputLimitExceeded { .. } | Self::TableOutputLimitExceeded { .. } => None,
        }
    }
}

/// Reads one bounded SQL batch to EOF and emits human-readable result tables.
///
/// `CREATE TABLE`, `DROP TABLE`, `TRUNCATE TABLE`, and `INSERT` statements are
/// silent. Each `SELECT`, `SHOW TABLES`, or `DESCRIBE TABLE` result is rendered
/// with the existing table formatter. Results remain in statement order and
/// are separated by one blank line.
pub fn run_table_batch(input: impl Read, output: impl Write) -> Result<(), BatchError> {
    run_table_batch_with_limit(input, output, DEFAULT_MAX_BATCH_BYTES)
}

/// Executes the human-readable table batch protocol with an explicit input
/// byte limit.
pub fn run_table_batch_with_limit(
    input: impl Read,
    output: impl Write,
    max_input_bytes: usize,
) -> Result<(), BatchError> {
    run_batch_with_limit(input, output, max_input_bytes, BatchOutputFormat::Table)
}

/// Reads one bounded SQL batch to EOF and emits CSVWithNames for every query.
///
/// `CREATE TABLE`, `DROP TABLE`, `TRUNCATE TABLE`, and `INSERT` statements are
/// silent. `SELECT`, `SHOW TABLES`, and `DESCRIBE TABLE` produce query results.
/// All statements share one in-memory catalog, and the SQL parser handles
/// semicolons inside string literals rather than splitting on raw bytes.
pub fn run_csv_batch(input: impl Read, output: impl Write) -> Result<(), BatchError> {
    run_batch_with_limit(
        input,
        output,
        DEFAULT_MAX_BATCH_BYTES,
        BatchOutputFormat::Csv,
    )
}

/// Executes the CSV batch protocol with an explicit input byte limit.
pub fn run_csv_batch_with_limit(
    input: impl Read,
    output: impl Write,
    max_input_bytes: usize,
) -> Result<(), BatchError> {
    run_batch_with_limit(input, output, max_input_bytes, BatchOutputFormat::Csv)
}

/// Reads one bounded SQL batch to EOF and emits one JSON object per query.
///
/// `CREATE TABLE`, `DROP TABLE`, `TRUNCATE TABLE`, and `INSERT` statements are
/// silent. Each `SELECT`, `SHOW TABLES`, or `DESCRIBE TABLE` result is rendered
/// on one line with column metadata and positional rows.
pub fn run_json_batch(input: impl Read, output: impl Write) -> Result<(), BatchError> {
    run_json_batch_with_limit(input, output, DEFAULT_MAX_BATCH_BYTES)
}

/// Executes the newline-delimited JSON batch protocol with an explicit input
/// byte limit.
pub fn run_json_batch_with_limit(
    input: impl Read,
    output: impl Write,
    max_input_bytes: usize,
) -> Result<(), BatchError> {
    run_batch_with_limit(input, output, max_input_bytes, BatchOutputFormat::Json)
}

#[derive(Debug, Clone, Copy)]
enum BatchOutputFormat {
    Table,
    Csv,
    Json,
}

fn run_batch_with_limit(
    input: impl Read,
    mut output: impl Write,
    max_input_bytes: usize,
    output_format: BatchOutputFormat,
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
    let mut emitted_table = false;
    for statement in statements {
        let result = database
            .execute_statement(statement)
            .map_err(BatchError::Sql)?;
        if let StatementResult::Query(query) = result {
            match output_format {
                BatchOutputFormat::Table => {
                    let prefix: &[u8] = if emitted_table { b"\n" } else { b"" };
                    write_table_with_affixes(
                        &mut output,
                        &query,
                        DEFAULT_MAX_TABLE_OUTPUT_BYTES,
                        prefix,
                        b"\n",
                    )
                    .map_err(batch_table_error)?;
                    emitted_table = true;
                }
                BatchOutputFormat::Csv => {
                    write_csv(&mut output, &query).map_err(BatchError::Write)?;
                }
                BatchOutputFormat::Json => {
                    write_json(&mut output, &query).map_err(BatchError::WriteJson)?;
                    output.write_all(b"\n").map_err(BatchError::WriteJson)?;
                }
            }
        }
    }
    Ok(())
}

fn batch_table_error(error: TableWriteError) -> BatchError {
    match error {
        TableWriteError::OutputLimitExceeded { bytes, max_bytes } => {
            BatchError::TableOutputLimitExceeded { bytes, max_bytes }
        }
        TableWriteError::Write(error) => BatchError::WriteTable(error),
    }
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
    fn json_batch_streams_multiple_results_on_separate_lines() {
        let input = b"CREATE TABLE t (n Int64);\n\
            INSERT INTO t VALUES (2), (1);\n\
            SELECT n FROM t ORDER BY n;\n\
            SELECT COUNT(*) AS rows FROM t;\n";
        let mut output = Vec::new();

        run_json_batch(&input[..], &mut output).expect("batch succeeds");

        assert_eq!(
            output,
            concat!(
                r#"{"columns":[{"name":"n","type":"Int64"}],"rows":[[1],[2]]}"#,
                "\n",
                r#"{"columns":[{"name":"rows","type":"Int64"}],"rows":[[2]]}"#,
                "\n"
            )
            .as_bytes()
        );
    }

    #[test]
    fn json_batch_preserves_typed_short_writer_failures() {
        let mut output = FailAfterBytes {
            remaining: 32,
            written: 0,
        };

        let error = run_json_batch(&b"SHOW TABLES;"[..], &mut output)
            .expect_err("writer stops during the JSON result");

        let BatchError::WriteJson(source) = error else {
            panic!("expected a typed JSON write error");
        };
        assert_eq!(source.kind(), io::ErrorKind::Other);
        assert_eq!(output.written, 32);
    }

    #[test]
    fn table_batch_preserves_typed_short_writer_failures() {
        let mut output = FailAfterBytes {
            remaining: 12,
            written: 0,
        };

        let error = run_table_batch(&b"SHOW TABLES;"[..], &mut output)
            .expect_err("writer stops during the table result");

        let BatchError::WriteTable(source) = error else {
            panic!("expected a typed table write error");
        };
        assert_eq!(source.kind(), io::ErrorKind::Other);
        assert_eq!(output.written, 12);
    }

    #[test]
    fn table_batch_rejects_wide_cell_padding_before_writing() {
        const ROWS: usize = 10_000;
        let wide_value = "x".repeat(10_000);
        let mut sql = format!(
            "CREATE TABLE padded (value String); INSERT INTO padded VALUES ('{wide_value}')"
        );
        for _ in 1..ROWS {
            sql.push_str(",('')");
        }
        sql.push_str("; SELECT value FROM padded;");
        let mut output = Vec::new();

        let error = run_table_batch(sql.as_bytes(), &mut output)
            .expect_err("alignment padding crosses the table output limit");

        let BatchError::TableOutputLimitExceeded { bytes, max_bytes } = error else {
            panic!("expected a typed table output limit error");
        };
        assert!(bytes > 100_000_000, "adversarial output was {bytes} bytes");
        assert_eq!(max_bytes, DEFAULT_MAX_TABLE_OUTPUT_BYTES);
        assert!(output.is_empty());
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
