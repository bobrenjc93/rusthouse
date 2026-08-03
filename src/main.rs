use std::env;
use std::ffi::OsStr;
use std::io::{self, BufReader, Write};
use std::process::ExitCode;

use rusthouse::Catalog;
use rusthouse::cli::{
    EXIT_INPUT_ERROR, EXIT_USAGE_ERROR, MAX_BATCH_BYTES, MAX_BATCH_STATEMENTS, MAX_STATEMENT_BYTES,
    execute_batch,
};

const HELP_START: &str = "RustHouse bounded CREATE/INSERT batch processor

Usage: rusthouse [--help]

Reads UTF-8 SQL from stdin, with one statement per nonempty line. All
statements execute in one in-memory catalog. Supported statement forms are
CREATE TABLE and INSERT INTO ... VALUES. Successful batches produce no output.

Options:
  -h, --help  Print this help

Limits:
";

const HELP_END: &str = "
Exit codes:
  0  success
  1  malformed input or statement execution error
  2  invalid command-line usage
  3  input limit exceeded
  4  unsupported statement
  5  stdin read error
";

fn main() -> ExitCode {
    let mut arguments = env::args_os().skip(1);
    match arguments.next() {
        None => run_batch(),
        Some(argument)
            if (argument == OsStr::new("--help") || argument == OsStr::new("-h"))
                && arguments.next().is_none() =>
        {
            write_help()
        }
        Some(_) => {
            eprintln!("rusthouse: invalid arguments; try 'rusthouse --help'");
            ExitCode::from(EXIT_USAGE_ERROR)
        }
    }
}

fn run_batch() -> ExitCode {
    let stdin = io::stdin();
    let mut catalog = Catalog::new();
    match execute_batch(BufReader::new(stdin.lock()), &mut catalog) {
        Ok(_) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rusthouse: {error}");
            ExitCode::from(error.exit_code())
        }
    }
}

fn write_help() -> ExitCode {
    let mut stdout = io::stdout().lock();
    let result = write!(
        stdout,
        "{HELP_START}  {MAX_STATEMENT_BYTES} bytes per statement\n  {MAX_BATCH_BYTES} bytes of stdin\n  {MAX_BATCH_STATEMENTS} statements per batch\n{HELP_END}"
    );
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::from(EXIT_INPUT_ERROR),
    }
}
