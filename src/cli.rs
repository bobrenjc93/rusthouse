//! Bounded, line-oriented command-line execution.

use std::error::Error;
use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::str::Utf8Error;

use crate::{Catalog, CatalogError, CatalogLimits, ParseLimits};

/// Default maximum number of bytes read during one CLI session.
pub const DEFAULT_MAX_SESSION_BYTES: usize = 64 * 1024;

/// Default maximum number of nonempty statements executed during one CLI session.
pub const DEFAULT_MAX_SESSION_STATEMENTS: usize = 1024;

/// Default maximum number of tables retained during one CLI session.
pub const DEFAULT_MAX_SESSION_TABLES: usize = 64;

/// Default maximum number of rows retained in each CLI table.
pub const DEFAULT_MAX_SESSION_ROWS_PER_TABLE: usize = 1024;

/// Resource bounds for one line-oriented CLI session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionLimits {
    /// Maximum total bytes read from standard input, including line endings.
    pub max_input_bytes: usize,
    /// Maximum number of nonempty statement lines.
    pub max_statements: usize,
    /// Maximum number of tables in the session catalog.
    pub max_tables: usize,
    /// Maximum number of rows in each table.
    pub max_rows_per_table: usize,
}

impl SessionLimits {
    /// Creates explicit input, statement, table, and per-table row bounds.
    pub const fn new(
        max_input_bytes: usize,
        max_statements: usize,
        max_tables: usize,
        max_rows_per_table: usize,
    ) -> Self {
        Self {
            max_input_bytes,
            max_statements,
            max_tables,
            max_rows_per_table,
        }
    }
}

impl Default for SessionLimits {
    fn default() -> Self {
        Self::new(
            DEFAULT_MAX_SESSION_BYTES,
            DEFAULT_MAX_SESSION_STATEMENTS,
            DEFAULT_MAX_SESSION_TABLES,
            DEFAULT_MAX_SESSION_ROWS_PER_TABLE,
        )
    }
}

/// A typed failure from a bounded CLI session.
#[derive(Debug)]
pub enum SessionError {
    /// Reading standard input failed.
    Read(io::Error),
    /// Writing a result to standard output failed.
    Write(io::Error),
    /// Standard input exceeded the total session byte bound.
    InputLimitExceeded { bytes: usize, max_bytes: usize },
    /// The number of nonempty lines exceeded the session statement bound.
    StatementLimitExceeded {
        line: usize,
        statements: usize,
        max_statements: usize,
    },
    /// A statement line was not valid UTF-8.
    InvalidUtf8 { line: usize, source: Utf8Error },
    /// The first word did not select a supported statement kind.
    UnsupportedStatement { line: usize, keyword: String },
    /// A supported statement failed during parsing or execution.
    Statement { line: usize, source: CatalogError },
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(error) => write!(formatter, "could not read standard input: {error}"),
            Self::Write(error) => write!(formatter, "could not write standard output: {error}"),
            Self::InputLimitExceeded { bytes, max_bytes } => write!(
                formatter,
                "session input has at least {bytes} bytes, exceeding the limit of {max_bytes} bytes"
            ),
            Self::StatementLimitExceeded {
                line,
                statements,
                max_statements,
            } => write!(
                formatter,
                "line {line} raises the session to {statements} statements, exceeding the limit of {max_statements}"
            ),
            Self::InvalidUtf8 { line, source } => {
                write!(formatter, "line {line} is not valid UTF-8: {source}")
            }
            Self::UnsupportedStatement { line, keyword } => {
                write!(
                    formatter,
                    "line {line} uses unsupported statement '{keyword}'"
                )
            }
            Self::Statement { line, source } => write!(formatter, "line {line}: {source}"),
        }
    }
}

impl Error for SessionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read(error) | Self::Write(error) => Some(error),
            Self::InvalidUtf8 { source, .. } => Some(source),
            Self::Statement { source, .. } => Some(source),
            Self::InputLimitExceeded { .. }
            | Self::StatementLimitExceeded { .. }
            | Self::UnsupportedStatement { .. } => None,
        }
    }
}

/// Runs one bounded stdin session, retaining a single in-memory catalog.
///
/// Each nonempty physical line must contain one `CREATE TABLE`, `INSERT INTO`,
/// or projection `SELECT`. Successful creates and inserts produce no output.
/// Each successful select produces one row-list line such as `[7, NULL, -2]`.
pub fn run_session(
    input: impl Read,
    mut output: impl Write,
    limits: SessionLimits,
) -> Result<(), SessionError> {
    let catalog_limits = CatalogLimits::new(limits.max_tables, limits.max_rows_per_table);
    let mut catalog = Catalog::new(catalog_limits);
    let parse_limits = ParseLimits::default();
    let read_limit = limits.max_input_bytes.saturating_add(1);
    let mut input = BufReader::new(input.take(read_limit as u64));
    let mut input_bytes = 0_usize;
    let mut statements = 0_usize;
    let mut line_number = 0_usize;
    let mut line = Vec::new();

    loop {
        line.clear();
        let bytes_read = input
            .read_until(b'\n', &mut line)
            .map_err(SessionError::Read)?;
        if bytes_read == 0 {
            break;
        }

        line_number = line_number.saturating_add(1);
        input_bytes = input_bytes.saturating_add(bytes_read);
        if input_bytes > limits.max_input_bytes {
            return Err(SessionError::InputLimitExceeded {
                bytes: limits.max_input_bytes.saturating_add(1),
                max_bytes: limits.max_input_bytes,
            });
        }

        let statement_bytes = strip_line_ending(&line);
        if statement_bytes.iter().all(u8::is_ascii_whitespace) {
            continue;
        }

        statements = statements.saturating_add(1);
        if statements > limits.max_statements {
            return Err(SessionError::StatementLimitExceeded {
                line: line_number,
                statements,
                max_statements: limits.max_statements,
            });
        }

        let statement =
            std::str::from_utf8(statement_bytes).map_err(|source| SessionError::InvalidUtf8 {
                line: line_number,
                source,
            })?;
        execute_line(
            &mut catalog,
            statement,
            line_number,
            parse_limits,
            &mut output,
        )?;
    }

    Ok(())
}

fn execute_line(
    catalog: &mut Catalog,
    statement: &str,
    line: usize,
    parse_limits: ParseLimits,
    output: &mut impl Write,
) -> Result<(), SessionError> {
    let keyword = statement
        .split_ascii_whitespace()
        .next()
        .expect("nonempty statements have a first word");

    if keyword.eq_ignore_ascii_case("CREATE") {
        catalog
            .execute_create(statement, parse_limits)
            .map_err(|source| SessionError::Statement { line, source })
    } else if keyword.eq_ignore_ascii_case("INSERT") {
        catalog
            .execute_insert(statement, parse_limits)
            .map_err(|source| SessionError::Statement { line, source })
    } else if keyword.eq_ignore_ascii_case("SELECT") {
        let rows = catalog
            .execute_select(statement, parse_limits)
            .map_err(|source| SessionError::Statement { line, source })?;
        write_rows(output, &rows).map_err(SessionError::Write)
    } else {
        Err(SessionError::UnsupportedStatement {
            line,
            keyword: keyword.to_owned(),
        })
    }
}

fn strip_line_ending(mut line: &[u8]) -> &[u8] {
    if let Some(without_lf) = line.strip_suffix(b"\n") {
        line = without_lf;
    }
    if let Some(without_cr) = line.strip_suffix(b"\r") {
        line = without_cr;
    }
    line
}

fn write_rows(output: &mut impl Write, rows: &[Option<i64>]) -> io::Result<()> {
    output.write_all(b"[")?;
    for (index, value) in rows.iter().enumerate() {
        if index != 0 {
            output.write_all(b", ")?;
        }
        match value {
            Some(value) => write!(output, "{value}")?,
            None => output.write_all(b"NULL")?,
        }
    }
    output.write_all(b"]\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CountingReader<'a> {
        bytes: &'a [u8],
        position: usize,
    }

    impl<'a> CountingReader<'a> {
        fn new(bytes: &'a [u8]) -> Self {
            Self { bytes, position: 0 }
        }

        fn remaining(&self) -> &'a [u8] {
            &self.bytes[self.position..]
        }
    }

    impl Read for CountingReader<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let bytes_read = buffer.len().min(self.remaining().len());
            buffer[..bytes_read].copy_from_slice(&self.remaining()[..bytes_read]);
            self.position += bytes_read;
            Ok(bytes_read)
        }
    }

    #[test]
    fn custom_session_limits_are_enforced_without_process_state() {
        let limits = SessionLimits::new(128, 2, 1, 1);
        let input = b"CREATE TABLE t (v Int64)\nINSERT INTO t VALUES (1)\nSELECT v FROM t\n";
        let error = run_session(&input[..], Vec::new(), limits).unwrap_err();

        assert!(matches!(
            error,
            SessionError::StatementLimitExceeded {
                line: 3,
                statements: 3,
                max_statements: 2,
            }
        ));
    }

    #[test]
    fn buffering_does_not_consume_past_the_input_detection_bound() {
        let mut input = CountingReader::new(b"0123456789");
        let limits = SessionLimits::new(0, 0, 0, 0);

        let error = run_session(&mut input, Vec::new(), limits).unwrap_err();

        assert!(matches!(
            error,
            SessionError::InputLimitExceeded {
                bytes: 1,
                max_bytes: 0,
            }
        ));
        assert_eq!(input.position, 1);
        assert_eq!(input.remaining(), b"123456789");
    }

    #[test]
    fn row_lists_have_stable_sql_null_rendering() {
        let mut output = Vec::new();
        let input = b"CREATE TABLE t (v Int64)\nINSERT INTO t VALUES (NULL)\nSELECT v FROM t\n";

        run_session(&input[..], &mut output, SessionLimits::default()).unwrap();

        assert_eq!(output, b"[NULL]\n");
    }
}
