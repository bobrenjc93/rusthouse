use std::env;
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::process::ExitCode;

use rusthouse::{MAX_INPUT_BYTES, execute_sql};

const HELP: &str = concat!(
    "RustHouse\n",
    "\n",
    "Usage: rusthouse [OPTIONS]\n",
    "\n",
    "Reads semicolon-separated literal SELECT statements from standard input.\n",
    "\n",
    "Options:\n",
    "      --format <FORMAT>  Output format [default: csv] [possible values: csv]\n",
    "  -h, --help             Print help\n",
);

enum Action {
    Help,
    Query,
}

fn main() -> ExitCode {
    let action = match parse_args(env::args_os().skip(1)) {
        Ok(action) => action,
        Err(message) => {
            eprintln!(
                "error: {message}\n\nUsage: rusthouse [OPTIONS]\n\nFor more information, try '--help'."
            );
            return ExitCode::from(2);
        }
    };

    match action {
        Action::Help => write_stdout(HELP),
        Action::Query => run_query(),
    }
}

fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Action, String> {
    let mut args = args.into_iter();
    let mut help = false;
    let mut format_seen = false;

    while let Some(argument) = args.next() {
        if argument == "-h" || argument == "--help" {
            help = true;
        } else if argument == "--format" {
            if format_seen {
                return Err("--format may only be specified once".to_owned());
            }
            format_seen = true;
            let value = args
                .next()
                .ok_or_else(|| "--format requires a value".to_owned())?;
            if value != "csv" {
                return Err(format!(
                    "unsupported format {:?}; the only supported format is csv",
                    value.to_string_lossy()
                ));
            }
        } else if argument.to_string_lossy().starts_with('-') {
            return Err(format!("unknown option {:?}", argument.to_string_lossy()));
        } else {
            return Err(format!(
                "unexpected argument {:?}; SQL is read from standard input",
                argument.to_string_lossy()
            ));
        }
    }

    Ok(if help { Action::Help } else { Action::Query })
}

fn run_query() -> ExitCode {
    let mut bytes = Vec::new();
    if let Err(error) = io::stdin()
        .lock()
        .take((MAX_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
    {
        eprintln!("error: failed to read SQL from stdin: {error}");
        return ExitCode::FAILURE;
    }

    if bytes.len() > MAX_INPUT_BYTES {
        eprintln!("error: SQL input exceeds the {MAX_INPUT_BYTES}-byte limit");
        return ExitCode::FAILURE;
    }

    let input = match String::from_utf8(bytes) {
        Ok(input) => input,
        Err(_) => {
            eprintln!("error: SQL input must be valid UTF-8");
            return ExitCode::FAILURE;
        }
    };

    match execute_sql(&input) {
        Ok(output) => write_stdout(&output),
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn write_stdout(output: &str) -> ExitCode {
    match io::stdout().lock().write_all(output.as_bytes()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: failed to write output: {error}");
            ExitCode::FAILURE
        }
    }
}
