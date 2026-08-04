use std::ffi::OsString;
use std::io::{self, Write};
use std::process::ExitCode;

const HELP: &str = "RustHouse line-oriented projection query session

Usage: rusthouse [OPTIONS]

Reads one CREATE TABLE, INSERT INTO, or projection SELECT per nonempty stdin line.
Successful SELECT statements print rows as [1, NULL, -2].

Limits:
  65536 input bytes, 1024 statements, 64 tables, 1024 rows per table

Options:
  -h, --help  Print help
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
        Err(argument) => fail(&format_args!(
            "unexpected argument '{}'; use --help for usage",
            argument.to_string_lossy()
        )),
    }
}

enum Action {
    Help,
    Run,
}

fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Action, OsString> {
    let mut args = args.into_iter();
    match (args.next(), args.next()) {
        (None, None) => Ok(Action::Run),
        (Some(argument), None) if argument == "--help" || argument == "-h" => Ok(Action::Help),
        (Some(argument), _) => Err(argument),
        (None, Some(_)) => unreachable!("an iterator cannot have a second item without a first"),
    }
}

fn fail(error: &dyn std::fmt::Display) -> ExitCode {
    eprintln!("error: {error}");
    ExitCode::FAILURE
}
