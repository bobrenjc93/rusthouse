use std::ffi::OsString;
use std::io::{self, Write};
use std::process::ExitCode;

const HELP: &str = "RustHouse bounded SQL query session

Usage: rusthouse [OPTIONS]

With no options, reads the legacy line-oriented Int64 session from stdin.
With --format csv or --format json, reads one semicolon-delimited SQL batch
through EOF and prints one result for each SELECT, SHOW TABLES, or DESCRIBE
TABLE query. CREATE, DROP, and INSERT remain silent.

Limits:
  legacy: 65536 input bytes, 1024 statements, 64 tables, 1024 rows per table
  SQL batch: 67108864 input bytes, 4096 statements
  batch INSERT ASTs: 100000 rows, 1000000 values
  batch schema/query AST lists: 100000 items
  batch SELECT: 10000 rows, 250000 values, 16777216 estimated result bytes
  batch grouped SELECT: 100000 groups, 500000 aggregate cells, 33554432 state bytes

Options:
  --format csv   Emit CSVWithNames-compatible query results
  --format json  Emit newline-delimited JSON query results
  -h, --help     Print help
";

fn main() -> ExitCode {
    match parse_args(std::env::args_os().skip(1)) {
        Ok(Action::Help) => match io::stdout().lock().write_all(HELP.as_bytes()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(&format_args!("could not write standard output: {error}")),
        },
        Ok(Action::Run) => {
            let stdin = io::stdin();
            let stdout = io::stdout();
            match rusthouse::run_session(
                stdin.lock(),
                stdout.lock(),
                rusthouse::SessionLimits::default(),
            ) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => fail(&error),
            }
        }
        Ok(Action::CsvBatch) => {
            let stdin = io::stdin();
            let stdout = io::stdout();
            match rusthouse::batch::run_csv_batch(stdin.lock(), stdout.lock()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => fail(&error),
            }
        }
        Ok(Action::JsonBatch) => {
            let stdin = io::stdin();
            let stdout = io::stdout();
            match rusthouse::batch::run_json_batch(stdin.lock(), stdout.lock()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => fail(&error),
            }
        }
        Err(argument) => fail(&format_args!(
            "unexpected argument '{}'; use --help for usage",
            argument.to_string_lossy()
        )),
    }
}

enum Action {
    Help,
    Run,
    CsvBatch,
    JsonBatch,
}

fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Action, OsString> {
    let mut args = args.into_iter();
    match (args.next(), args.next(), args.next()) {
        (None, None, None) => Ok(Action::Run),
        (Some(argument), None, None) if argument == "--help" || argument == "-h" => {
            Ok(Action::Help)
        }
        (Some(format), Some(value), None) if format == "--format" && value == "csv" => {
            Ok(Action::CsvBatch)
        }
        (Some(format), Some(value), None) if format == "--format" && value == "json" => {
            Ok(Action::JsonBatch)
        }
        (Some(argument), _, _) => Err(argument),
        (None, Some(_), _) | (None, None, Some(_)) => {
            unreachable!("an iterator cannot skip an argument")
        }
    }
}

fn fail(error: &dyn std::fmt::Display) -> ExitCode {
    eprintln!("error: {error}");
    ExitCode::FAILURE
}
