use std::env;
use std::error::Error;
use std::fmt;
use std::io::{self, Read, Write};
use std::process::ExitCode;

use rusthouse::lexer::LexerLimits;
use rusthouse::{CsvWithNamesWriter, Database, SelectResult, Value};

const HELP: &str = "RustHouse executes a bounded SQL script from standard input.

Usage: rusthouse --format csv

Options:
      --format <FORMAT>  Output format [possible values: csv]
  -h, --help             Print help
";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rusthouse: {error}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<(), CliError> {
    match parse_args(env::args().skip(1))? {
        Action::Help => {
            print!("{HELP}");
            io::stdout().flush()?;
            Ok(())
        }
        Action::Execute => execute_stdin(),
    }
}

fn execute_stdin() -> Result<(), CliError> {
    let max_input_bytes = LexerLimits::default().max_input_bytes;
    let mut input = Vec::new();
    io::stdin()
        .lock()
        .take(max_input_bytes as u64 + 1)
        .read_to_end(&mut input)?;
    if input.len() > max_input_bytes {
        return Err(CliError::InputTooLarge {
            limit: max_input_bytes,
        });
    }
    let input = std::str::from_utf8(&input).map_err(|_| CliError::InvalidUtf8)?;
    let results = Database::new().execute(input)?;

    // Complete execution and formatting before stdout is touched so failures
    // cannot leave behind an earlier result set or a partial row.
    let mut output = Vec::new();
    for result in results {
        match result {
            SelectResult::Scalar(result) => {
                let mut csv = CsvWithNamesWriter::new(&mut output, [result.column_name()])?;
                csv.write_row([result.value_text()])?;
                csv.flush()?;
            }
            SelectResult::Table(result) => {
                let mut csv = CsvWithNamesWriter::new(
                    &mut output,
                    result.headers().iter().map(|column| column.name()),
                )?;
                for row in result.rows() {
                    let row = row.iter().map(value_text).collect::<Vec<_>>();
                    csv.write_row(row)?;
                }
                csv.flush()?;
            }
        }
    }

    let mut stdout = io::stdout().lock();
    stdout.write_all(&output)?;
    stdout.flush()?;
    Ok(())
}

fn value_text(value: &Value) -> String {
    match value {
        Value::Int64(value) => value.to_string(),
        Value::Float64(value) => value.to_string(),
        Value::Bool(value) => value.to_string(),
        Value::String(value) => value.clone(),
    }
}

enum Action {
    Help,
    Execute,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Action, CliError> {
    let mut args = args.into_iter();
    let mut format = None;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "-h" | "--help" => return Ok(Action::Help),
            "--format" => {
                if format.is_some() {
                    return Err(CliError::Arguments("--format may only be supplied once"));
                }
                format = Some(
                    args.next()
                        .ok_or(CliError::Arguments("--format requires a value"))?,
                );
            }
            _ => {
                if let Some(value) = argument.strip_prefix("--format=") {
                    if format.replace(value.to_owned()).is_some() {
                        return Err(CliError::Arguments("--format may only be supplied once"));
                    }
                } else {
                    return Err(CliError::UnknownArgument(argument));
                }
            }
        }
    }

    match format.as_deref() {
        Some("csv") => Ok(Action::Execute),
        Some(_) => Err(CliError::Arguments("only --format csv is supported")),
        None => Err(CliError::Arguments("--format csv is required")),
    }
}

#[derive(Debug)]
enum CliError {
    Arguments(&'static str),
    UnknownArgument(String),
    InputTooLarge { limit: usize },
    InvalidUtf8,
    Database(rusthouse::DatabaseError),
    Csv(rusthouse::CsvWithNamesError),
    Io(io::Error),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Arguments(message) => formatter.write_str(message),
            Self::UnknownArgument(argument) => write!(formatter, "unknown argument `{argument}`"),
            Self::InputTooLarge { limit } => {
                write!(formatter, "SQL input exceeds the {limit}-byte limit")
            }
            Self::InvalidUtf8 => formatter.write_str("SQL input is not valid UTF-8"),
            Self::Database(error) => error.fmt(formatter),
            Self::Csv(error) => error.fmt(formatter),
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
        }
    }
}

impl Error for CliError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Csv(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusthouse::DatabaseError> for CliError {
    fn from(error: rusthouse::DatabaseError) -> Self {
        Self::Database(error)
    }
}

impl From<rusthouse::CsvWithNamesError> for CliError {
    fn from(error: rusthouse::CsvWithNamesError) -> Self {
        Self::Csv(error)
    }
}

impl From<io::Error> for CliError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
