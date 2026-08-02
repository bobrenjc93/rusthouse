use std::env;
use std::ffi::OsString;
use std::fmt;
use std::io::{self, BufWriter, Read, Write};
use std::process::ExitCode;

use rusthouse::{Database, Error, ExecutionResult};

const HELP: &str = "RustHouse SQL CLI\n\nUsage: rusthouse [--format csv]\n\nReads a SQL batch from stdin and writes SELECT results to stdout.\n";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), CliError> {
    if parse_args(env::args_os().skip(1))? == Action::Help {
        print!("{HELP}");
        return Ok(());
    }

    let mut database = Database::new();
    let input = read_complete_stdin(database.config().max_input_bytes)?;
    let results = database.execute_batch_iter(&input)?;

    let stdout = io::stdout();
    let mut writer = BufWriter::new(stdout.lock());
    for result in results {
        let result = result?;
        if let ExecutionResult::Query(result) = result {
            rusthouse::csv::write_query(&mut writer, &result).map_err(CliError::Output)?;
        }
    }
    writer.flush().map_err(CliError::Output)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Action {
    Run,
    Help,
}

fn parse_args(arguments: impl IntoIterator<Item = OsString>) -> Result<Action, CliError> {
    let mut arguments = arguments.into_iter();
    let mut format_seen = false;
    while let Some(argument) = arguments.next() {
        let argument = argument
            .into_string()
            .map_err(|_| CliError::Argument("arguments must be valid UTF-8".to_owned()))?;
        match argument.as_str() {
            "-h" | "--help" => return Ok(Action::Help),
            "--format" => {
                if format_seen {
                    return Err(CliError::Argument(
                        "--format may only be specified once".to_owned(),
                    ));
                }
                let value = arguments
                    .next()
                    .ok_or_else(|| CliError::Argument("--format requires a value".to_owned()))?;
                let value = value
                    .into_string()
                    .map_err(|_| CliError::Argument("the format must be valid UTF-8".to_owned()))?;
                validate_format(&value)?;
                format_seen = true;
            }
            value if value.starts_with("--format=") => {
                if format_seen {
                    return Err(CliError::Argument(
                        "--format may only be specified once".to_owned(),
                    ));
                }
                validate_format(&value["--format=".len()..])?;
                format_seen = true;
            }
            _ => {
                return Err(CliError::Argument(format!(
                    "unknown argument {argument:?}; use --help for usage"
                )));
            }
        }
    }
    Ok(Action::Run)
}

fn validate_format(value: &str) -> Result<(), CliError> {
    if value.eq_ignore_ascii_case("csv") {
        Ok(())
    } else {
        Err(CliError::Argument(format!(
            "unsupported format {value:?}; expected csv"
        )))
    }
}

fn read_complete_stdin(maximum: usize) -> Result<String, CliError> {
    let stdin = io::stdin();
    read_bounded(stdin.lock(), maximum)
}

fn read_bounded(mut reader: impl Read, maximum: usize) -> Result<String, CliError> {
    let mut retained = Vec::with_capacity(maximum.min(8192));
    let mut buffer = [0_u8; 8192];

    loop {
        let remaining = maximum - retained.len();
        let requested = remaining.saturating_add(1).min(buffer.len());
        let read = reader
            .read(&mut buffer[..requested])
            .map_err(CliError::Input)?;
        if read == 0 {
            break;
        }
        if read > remaining {
            return Err(CliError::Database(Error::InputTooLarge {
                actual: maximum.saturating_add(1),
                maximum,
            }));
        }
        retained.extend_from_slice(&buffer[..read]);
    }

    String::from_utf8(retained).map_err(|_| CliError::InvalidUtf8)
}

#[derive(Debug)]
enum CliError {
    Argument(String),
    Input(io::Error),
    Output(io::Error),
    InvalidUtf8,
    Database(Error),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Argument(message) => formatter.write_str(message),
            Self::Input(error) => write!(formatter, "failed to read stdin: {error}"),
            Self::Output(error) => write!(formatter, "failed to write stdout: {error}"),
            Self::InvalidUtf8 => formatter.write_str("stdin is not valid UTF-8"),
            Self::Database(error) => error.fmt(formatter),
        }
    }
}

impl From<Error> for CliError {
    fn from(error: Error) -> Self {
        Self::Database(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_csv_format_forms_and_rejects_other_formats() {
        assert_eq!(
            parse_args([OsString::from("--format"), OsString::from("csv")]).unwrap(),
            Action::Run
        );
        assert_eq!(
            parse_args([OsString::from("--format=CSV")]).unwrap(),
            Action::Run
        );
        assert!(parse_args([OsString::from("--format=json")]).is_err());
    }

    #[test]
    fn bounded_reader_stops_at_the_first_excess_byte() {
        struct NeverReadPastLimit {
            first: Option<Vec<u8>>,
        }

        impl Read for NeverReadPastLimit {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                let input = self
                    .first
                    .take()
                    .expect("reader must return immediately after detecting overflow");
                let read = input.len().min(buffer.len());
                buffer[..read].copy_from_slice(&input[..read]);
                Ok(read)
            }
        }

        let error = read_bounded(
            NeverReadPastLimit {
                first: Some(b"12345".to_vec()),
            },
            4,
        )
        .expect_err("five bytes exceed a four-byte limit");
        assert!(matches!(
            error,
            CliError::Database(Error::InputTooLarge {
                actual: 5,
                maximum: 4
            })
        ));
    }
}
