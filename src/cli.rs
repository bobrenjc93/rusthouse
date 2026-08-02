//! Command-line argument, input, and output handling.

use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::io::{self, Read, Write};

use crate::query::QueryError;

/// Maximum accepted SQL input size in bytes.
pub const MAX_QUERY_BYTES: usize = 1024 * 1024;

/// Command-line help shown by `rusthouse --help`.
pub const HELP: &str = concat!(
    "RustHouse ",
    env!("CARGO_PKG_VERSION"),
    "\nExecute constant SELECT queries.\n\n",
    "Usage: rusthouse [OPTIONS]\n\n",
    "Options:\n",
    "  -e, --execute <SQL>    Execute SQL instead of reading standard input\n",
    "      --format <FORMAT>  Output format [default: csv] [possible values: csv]\n",
    "  -h, --help             Print help\n\n",
    "SQL is limited to semicolon-separated SELECT statements projecting Int64,\n",
    "Float64, Bool, and String literals, with optional AS aliases. Without\n",
    "--execute, valid input is read from standard input through EOF. Input over\n",
    "1 MiB is rejected as soon as the limit is crossed. Tables, clauses,\n",
    "expressions, aggregation, and DDL are not supported.\n"
);

/// An argument, input, query, or output failure at the CLI boundary.
#[derive(Debug)]
pub enum CliError {
    Argument(String),
    Input(io::Error),
    InputEncoding,
    InputTooLarge,
    Query(QueryError),
    Output(io::Error),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Argument(message) => write!(formatter, "argument error: {message}"),
            Self::Input(error) => write!(formatter, "failed to read standard input: {error}"),
            Self::InputEncoding => write!(formatter, "SQL input is not valid UTF-8"),
            Self::InputTooLarge => write!(
                formatter,
                "SQL input exceeds the {MAX_QUERY_BYTES}-byte limit"
            ),
            Self::Query(error) => error.fmt(formatter),
            Self::Output(error) => write!(formatter, "failed to write CSV output: {error}"),
        }
    }
}

impl Error for CliError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Input(error) | Self::Output(error) => Some(error),
            Self::Query(error) => Some(error),
            _ => None,
        }
    }
}

/// Runs one CLI invocation against supplied streams.
pub fn run<I, T>(arguments: I, input: impl Read, mut output: impl Write) -> Result<(), CliError>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    match Options::parse(arguments)? {
        Action::Help => output.write_all(HELP.as_bytes()).map_err(CliError::Output),
        Action::Execute(options) => {
            let sql = match options.sql {
                Some(sql) => {
                    if sql.len() > MAX_QUERY_BYTES {
                        return Err(CliError::InputTooLarge);
                    }
                    sql
                }
                None => read_sql(input)?,
            };
            let results = crate::query::execute(&sql).map_err(CliError::Query)?;
            crate::csv::write_results(&results, &mut output).map_err(CliError::Output)
        }
    }
}

struct Options {
    sql: Option<String>,
}

enum Action {
    Help,
    Execute(Options),
}

impl Options {
    fn parse<I, T>(arguments: I) -> Result<Action, CliError>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString>,
    {
        let arguments = arguments
            .into_iter()
            .map(|argument| {
                argument
                    .into()
                    .into_string()
                    .map_err(|_| CliError::Argument("arguments must be valid UTF-8".to_owned()))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let mut sql = None;
        let mut format_seen = false;
        let mut index = 0;
        while index < arguments.len() {
            let argument = &arguments[index];
            match argument.as_str() {
                "-h" | "--help" => return Ok(Action::Help),
                "-e" | "--execute" => {
                    let value = arguments.get(index + 1).ok_or_else(|| {
                        CliError::Argument(format!("{argument} requires a SQL value"))
                    })?;
                    set_once(&mut sql, value.clone(), "--execute")?;
                    index += 2;
                }
                "--format" => {
                    let value = arguments.get(index + 1).ok_or_else(|| {
                        CliError::Argument("--format requires a value".to_owned())
                    })?;
                    validate_format(value, &mut format_seen)?;
                    index += 2;
                }
                _ if argument.starts_with("--execute=") => {
                    let value = argument.trim_start_matches("--execute=").to_owned();
                    set_once(&mut sql, value, "--execute")?;
                    index += 1;
                }
                _ if argument.starts_with("--format=") => {
                    let value = argument.trim_start_matches("--format=");
                    validate_format(value, &mut format_seen)?;
                    index += 1;
                }
                _ => {
                    return Err(CliError::Argument(format!(
                        "unexpected argument {argument:?}; use --help for usage"
                    )));
                }
            }
        }

        Ok(Action::Execute(Self { sql }))
    }
}

fn set_once(destination: &mut Option<String>, value: String, option: &str) -> Result<(), CliError> {
    if destination.replace(value).is_some() {
        return Err(CliError::Argument(format!(
            "{option} may only be specified once"
        )));
    }
    Ok(())
}

fn validate_format(format: &str, seen: &mut bool) -> Result<(), CliError> {
    if *seen {
        return Err(CliError::Argument(
            "--format may only be specified once".to_owned(),
        ));
    }
    *seen = true;
    if format != "csv" {
        return Err(CliError::Argument(format!(
            "unsupported format {format:?}; expected csv"
        )));
    }
    Ok(())
}

fn read_sql(mut input: impl Read) -> Result<String, CliError> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8192];

    loop {
        let read = input.read(&mut buffer).map_err(CliError::Input)?;
        if read == 0 {
            break;
        }
        if read > MAX_QUERY_BYTES - bytes.len() {
            return Err(CliError::InputTooLarge);
        }
        bytes.extend_from_slice(&buffer[..read]);
    }

    String::from_utf8(bytes).map_err(|_| CliError::InputEncoding)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_equals_options() {
        let mut output = Vec::new();
        run(
            ["--execute=SELECT 1 AS value", "--format=csv"],
            io::empty(),
            &mut output,
        )
        .unwrap();

        assert_eq!(output, b"\"value\"\n1\n");
    }

    #[test]
    fn stops_reading_as_soon_as_input_is_oversized() {
        struct TrackingInput {
            remaining: usize,
            reached_eof: bool,
        }

        impl Read for TrackingInput {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                if self.remaining == 0 {
                    self.reached_eof = true;
                    return Ok(0);
                }
                let read = self.remaining.min(buffer.len());
                buffer[..read].fill(b' ');
                self.remaining -= read;
                Ok(read)
            }
        }

        let mut input = TrackingInput {
            remaining: MAX_QUERY_BYTES + 16 * 1024,
            reached_eof: false,
        };
        let error = read_sql(&mut input).unwrap_err();

        assert!(matches!(error, CliError::InputTooLarge));
        assert!(!input.reached_eof);
        assert_eq!(input.remaining, 8 * 1024);
    }
}
