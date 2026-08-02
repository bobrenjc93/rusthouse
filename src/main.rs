use std::env;
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::process::ExitCode;

use rusthouse::{MAX_SQL_INPUT_BYTES, execute_literal_select_csv};

const USAGE: &str = "Usage: rusthouse [OPTIONS]";
const HELP: &str = concat!(
    "rusthouse ",
    env!("CARGO_PKG_VERSION"),
    "\n",
    env!("CARGO_PKG_DESCRIPTION"),
    "\n\n",
    "Usage: rusthouse [OPTIONS]\n\n",
    "Options:\n",
    "  -e, --execute <SQL>    Execute SQL instead of reading stdin\n",
    "      --format <FORMAT>  Output format [default: csv] [possible value: csv]\n",
    "  -h, --help             Print help\n",
);

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let mut stderr = io::stderr().lock();
            match error {
                AppError::Arguments(message) => {
                    let _ = writeln!(
                        stderr,
                        "error: {message}\n\n{USAGE}\n\nFor more information, try '--help'."
                    );
                }
                AppError::Message(message) => {
                    let _ = writeln!(stderr, "error: {message}");
                }
            }
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), AppError> {
    match parse_arguments(env::args_os().skip(1))? {
        Command::Help => write_stdout(HELP),
        Command::Execute { sql } => {
            let sql = match sql {
                Some(sql) => {
                    if sql.len() > MAX_SQL_INPUT_BYTES {
                        return Err(input_too_large());
                    }
                    sql
                }
                None => read_bounded_sql(io::stdin().lock())?,
            };
            let csv = execute_literal_select_csv(&sql)
                .map_err(|error| AppError::Message(error.to_string()))?;
            write_stdout(&csv)
        }
    }
}

fn write_stdout(contents: &str) -> Result<(), AppError> {
    io::stdout()
        .lock()
        .write_all(contents.as_bytes())
        .map_err(|error| AppError::Message(format!("failed to write stdout: {error}")))
}

enum Command {
    Help,
    Execute { sql: Option<String> },
}

#[derive(Debug)]
enum AppError {
    Arguments(String),
    Message(String),
}

fn parse_arguments(arguments: impl Iterator<Item = OsString>) -> Result<Command, AppError> {
    let mut arguments = arguments;
    let mut sql = None;
    let mut format_seen = false;

    while let Some(argument) = arguments.next() {
        let argument = argument.into_string().map_err(|_| {
            AppError::Arguments("command-line arguments must be valid UTF-8".to_owned())
        })?;

        match argument.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "-e" | "--execute" => {
                if sql.is_some() {
                    return Err(AppError::Arguments(
                        "the argument '--execute' cannot be used more than once".to_owned(),
                    ));
                }
                let value = arguments.next().ok_or_else(|| {
                    AppError::Arguments("'--execute' requires a SQL value".to_owned())
                })?;
                sql = Some(value.into_string().map_err(|_| {
                    AppError::Arguments("SQL passed to '--execute' must be valid UTF-8".to_owned())
                })?);
            }
            "--format" => {
                if format_seen {
                    return Err(AppError::Arguments(
                        "the argument '--format' cannot be used more than once".to_owned(),
                    ));
                }
                let value = arguments
                    .next()
                    .ok_or_else(|| AppError::Arguments("'--format' requires a value".to_owned()))?;
                validate_format(value)?;
                format_seen = true;
            }
            _ if argument.starts_with("--execute=") => {
                if sql.is_some() {
                    return Err(AppError::Arguments(
                        "the argument '--execute' cannot be used more than once".to_owned(),
                    ));
                }
                sql = Some(argument["--execute=".len()..].to_owned());
            }
            _ if argument.starts_with("--format=") => {
                if format_seen {
                    return Err(AppError::Arguments(
                        "the argument '--format' cannot be used more than once".to_owned(),
                    ));
                }
                validate_format(OsString::from(&argument["--format=".len()..]))?;
                format_seen = true;
            }
            _ => {
                return Err(AppError::Arguments(format!(
                    "unexpected argument '{argument}'"
                )));
            }
        }
    }

    Ok(Command::Execute { sql })
}

fn validate_format(value: OsString) -> Result<(), AppError> {
    let value = value
        .into_string()
        .map_err(|_| AppError::Arguments("format must be valid UTF-8".to_owned()))?;
    if value == "csv" {
        Ok(())
    } else {
        Err(AppError::Arguments(format!(
            "unsupported format '{value}'; only 'csv' is available"
        )))
    }
}

fn read_bounded_sql(mut input: impl Read) -> Result<String, AppError> {
    let mut sql = Vec::with_capacity(8 * 1024);
    let mut buffer = [0; 8 * 1024];
    let mut too_large = false;

    loop {
        let bytes_read = input.read(&mut buffer).map_err(|error| {
            AppError::Message(format!("failed to read SQL from stdin: {error}"))
        })?;
        if bytes_read == 0 {
            break;
        }

        if !too_large {
            let remaining = MAX_SQL_INPUT_BYTES - sql.len();
            let bytes_to_keep = bytes_read.min(remaining);
            sql.extend_from_slice(&buffer[..bytes_to_keep]);
            too_large = bytes_read > remaining;
        }
    }

    if too_large {
        return Err(input_too_large());
    }
    String::from_utf8(sql)
        .map_err(|_| AppError::Message("SQL input must be valid UTF-8".to_owned()))
}

fn input_too_large() -> AppError {
    AppError::Message(format!(
        "SQL input exceeds the {MAX_SQL_INPUT_BYTES}-byte limit"
    ))
}
