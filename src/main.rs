use std::env;
use std::ffi::OsString;
use std::fmt;
use std::io::{self, BufWriter, Read, Write};
use std::process::ExitCode;

use rusthouse::{MAX_QUERY_BYTES, execute, write_csv};

const HELP: &str = "\
RustHouse literal query CLI

Usage: rusthouse [OPTIONS]

Reads one SQL statement from standard input and writes its result.

Options:
      --format <FORMAT>  Output format [default: csv] [possible values: csv]
  -h, --help             Print help
";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rusthouse: {error}");
            error.exit_code()
        }
    }
}

fn run() -> Result<(), CliError> {
    match parse_args(env::args_os().skip(1))? {
        Action::Help => {
            let mut output = BufWriter::new(io::stdout().lock());
            output
                .write_all(HELP.as_bytes())
                .map_err(CliError::Output)?;
            output.flush().map_err(CliError::Output)
        }
        Action::Execute(Format::Csv) => {
            let query = read_query(io::stdin().lock())?;
            let result = execute(&query).map_err(CliError::Query)?;
            let mut output = BufWriter::new(io::stdout().lock());
            write_csv(&result, &mut output).map_err(CliError::Output)?;
            output.flush().map_err(CliError::Output)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Csv,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Help,
    Execute(Format),
}

fn parse_args(arguments: impl IntoIterator<Item = OsString>) -> Result<Action, CliError> {
    let mut arguments = arguments.into_iter();
    let mut format = None;

    while let Some(argument) = arguments.next() {
        let argument = argument
            .into_string()
            .map_err(|_| CliError::Arguments("arguments must be valid UTF-8".to_owned()))?;
        match argument.as_str() {
            "-h" | "--help" => return Ok(Action::Help),
            "--format" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| CliError::Arguments("--format requires a value".to_owned()))?;
                let value = value
                    .into_string()
                    .map_err(|_| CliError::Arguments("format must be valid UTF-8".to_owned()))?;
                set_format(&mut format, &value)?;
            }
            _ if argument.starts_with("--format=") => {
                let value = &argument["--format=".len()..];
                set_format(&mut format, value)?;
            }
            _ => {
                return Err(CliError::Arguments(format!(
                    "unsupported option or argument: {argument}"
                )));
            }
        }
    }

    Ok(Action::Execute(format.unwrap_or(Format::Csv)))
}

fn set_format(format: &mut Option<Format>, value: &str) -> Result<(), CliError> {
    if format.is_some() {
        return Err(CliError::Arguments(
            "--format may only be provided once".to_owned(),
        ));
    }
    if value != "csv" {
        return Err(CliError::Arguments(format!(
            "unsupported output format: {value}"
        )));
    }
    *format = Some(Format::Csv);
    Ok(())
}

fn read_query(reader: impl Read) -> Result<String, CliError> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_QUERY_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(CliError::Input)?;
    if bytes.len() > MAX_QUERY_BYTES {
        return Err(CliError::InputTooLarge {
            limit: MAX_QUERY_BYTES,
        });
    }
    String::from_utf8(bytes).map_err(|_| CliError::InvalidUtf8)
}

#[derive(Debug)]
enum CliError {
    Arguments(String),
    Input(io::Error),
    InputTooLarge { limit: usize },
    InvalidUtf8,
    Query(rusthouse::QueryError),
    Output(io::Error),
}

impl CliError {
    fn exit_code(&self) -> ExitCode {
        match self {
            Self::Arguments(_) => ExitCode::from(2),
            Self::Input(_)
            | Self::InputTooLarge { .. }
            | Self::InvalidUtf8
            | Self::Query(_)
            | Self::Output(_) => ExitCode::FAILURE,
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Arguments(message) => write!(formatter, "{message}; try --help"),
            Self::Input(error) => write!(formatter, "failed to read standard input: {error}"),
            Self::InputTooLarge { limit } => {
                write!(formatter, "query exceeds the {limit}-byte input limit")
            }
            Self::InvalidUtf8 => formatter.write_str("query input is not valid UTF-8"),
            Self::Query(error) => write!(formatter, "{error}"),
            Self::Output(error) => write!(formatter, "failed to write standard output: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_csv() {
        assert!(matches!(parse_args([]), Ok(Action::Execute(Format::Csv))));
    }

    #[test]
    fn recognizes_equals_format_syntax() {
        assert!(matches!(
            parse_args([OsString::from("--format=csv")]),
            Ok(Action::Execute(Format::Csv))
        ));
    }
}
