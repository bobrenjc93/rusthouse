use std::env;
use std::io::{self, Read, Write};
use std::process::ExitCode;

use rusthouse::{Database, ExecutionResult, write_csv};

const HELP: &str = "RustHouse in-memory analytical SQL engine

Usage: rusthouse [OPTIONS]

Reads one or more SQL statements from standard input.

Options:
      --format <FORMAT>  Output format [default: csv] [possible values: csv]
  -h, --help             Print help
";

enum Action {
    Help,
    Run,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "rusthouse: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> io::Result<()> {
    match parse_arguments(env::args().skip(1))? {
        Action::Help => {
            io::stdout().lock().write_all(HELP.as_bytes())?;
            return Ok(());
        }
        Action::Run => {}
    }

    let mut input = String::new();
    io::stdin().lock().read_to_string(&mut input)?;
    let mut database = Database::new();
    let results = database
        .execute(&input)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let mut stdout = io::stdout().lock();
    for result in results {
        if let ExecutionResult::Query(result) = result {
            write_csv(&mut stdout, &result)?;
        }
    }
    stdout.flush()
}

fn parse_arguments(arguments: impl IntoIterator<Item = String>) -> io::Result<Action> {
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "-h" | "--help" => return Ok(Action::Help),
            "--format" => {
                let value = arguments.next().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "--format requires a value")
                })?;
                ensure_csv(&value)?;
            }
            value if value.starts_with("--format=") => {
                ensure_csv(&value["--format=".len()..])?;
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown argument {argument:?}; use --help"),
                ));
            }
        }
    }
    Ok(Action::Run)
}

fn ensure_csv(value: &str) -> io::Result<()> {
    if value.eq_ignore_ascii_case("csv") {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported format {value:?}; expected csv"),
        ))
    }
}
