use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{self, BufReader, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use rusthouse::Catalog;
use rusthouse::cli::{
    EXIT_EXECUTION_ERROR, EXIT_OUTPUT_ERROR, EXIT_USAGE_ERROR, MAX_BATCH_BYTES,
    MAX_BATCH_STATEMENTS, MAX_STATEMENT_BYTES, execute_batch_with_output,
};
use rusthouse::snapshot::{DEFAULT_MAX_PAYLOAD_LEN, SnapshotStore};

const HELP_START: &str = "RustHouse bounded SQL batch processor

Usage: rusthouse [--format csv] [--load-table NAME=PATH] [--save-table NAME=PATH]
       rusthouse [--help]

Reads UTF-8 SQL from stdin, with one statement per nonempty line. All
statements execute in one in-memory catalog. Supported statement forms are
CREATE TABLE, INSERT INTO ... VALUES, and SELECT. Each SELECT result is written
to stdout as CSVWithNames; CREATE and INSERT produce no output. A table snapshot
is loaded before stdin is read and saved only after the complete batch succeeds.

Options:
  --format csv            Write SELECT results as CSVWithNames (the default)
  --load-table NAME=PATH  Load one table snapshot before processing stdin
  --save-table NAME=PATH  Atomically save one table after a successful batch
  -h, --help              Print this help

Limits:
";

const HELP_END: &str = "
Exit codes:
  0  success
  1  malformed input, statement execution, or snapshot error
  2  invalid command-line usage
  3  input limit exceeded
  4  unsupported statement
  5  stdin read error
  6  stdout write error
";

fn main() -> ExitCode {
    match parse_arguments(env::args_os().skip(1)) {
        Ok(Action::Run(options)) => run_batch(options),
        Ok(Action::Help) => write_help(),
        Err(()) => {
            eprintln!("rusthouse: invalid arguments; try 'rusthouse --help'");
            ExitCode::from(EXIT_USAGE_ERROR)
        }
    }
}

fn run_batch(options: Options) -> ExitCode {
    let snapshots = SnapshotStore::default();
    let mut catalog = Catalog::new();

    if let Some(table) = &options.load_table
        && let Err(error) = catalog.load_table(&table.name, &table.path, &snapshots)
    {
        eprintln!(
            "rusthouse: could not load table `{}` from {}: {error}",
            table.name,
            table.path.display()
        );
        return ExitCode::from(EXIT_EXECUTION_ERROR);
    }

    let stdin = io::stdin();
    let stdout = io::stdout();
    match execute_batch_with_output(
        BufReader::new(stdin.lock()),
        &mut catalog,
        &mut stdout.lock(),
    ) {
        Ok(_) => {
            if let Some(table) = &options.save_table
                && let Err(error) = catalog.save_table(&table.name, &table.path, &snapshots)
            {
                eprintln!(
                    "rusthouse: could not save table `{}` to {}: {error}",
                    table.name,
                    table.path.display()
                );
                return ExitCode::from(EXIT_EXECUTION_ERROR);
            }
            ExitCode::SUCCESS
        }
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
        "{HELP_START}  {MAX_STATEMENT_BYTES} bytes per statement\n  {MAX_BATCH_BYTES} bytes of stdin\n  {MAX_BATCH_STATEMENTS} statements per batch\n  {DEFAULT_MAX_PAYLOAD_LEN} bytes per snapshot payload\n{HELP_END}"
    );
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::from(EXIT_OUTPUT_ERROR),
    }
}

#[derive(Debug, Default)]
struct Options {
    load_table: Option<TableSnapshotArgument>,
    save_table: Option<TableSnapshotArgument>,
}

#[derive(Debug)]
struct TableSnapshotArgument {
    name: String,
    path: PathBuf,
}

enum Action {
    Run(Options),
    Help,
}

fn parse_arguments(arguments: impl Iterator<Item = OsString>) -> Result<Action, ()> {
    let mut arguments = arguments.peekable();
    if arguments
        .peek()
        .is_some_and(|argument| argument == OsStr::new("--help") || argument == OsStr::new("-h"))
    {
        arguments.next();
        return if arguments.next().is_none() {
            Ok(Action::Help)
        } else {
            Err(())
        };
    }

    let mut options = Options::default();
    let mut format_seen = false;
    while let Some(option) = arguments.next() {
        if option == OsStr::new("--format") {
            if format_seen || arguments.next().as_deref() != Some(OsStr::new("csv")) {
                return Err(());
            }
            format_seen = true;
        } else if option == OsStr::new("--load-table") {
            if options.load_table.is_some() {
                return Err(());
            }
            options.load_table = Some(parse_table_snapshot_argument(arguments.next().ok_or(())?)?);
        } else if option == OsStr::new("--save-table") {
            if options.save_table.is_some() {
                return Err(());
            }
            options.save_table = Some(parse_table_snapshot_argument(arguments.next().ok_or(())?)?);
        } else {
            return Err(());
        }
    }

    Ok(Action::Run(options))
}

fn parse_table_snapshot_argument(argument: OsString) -> Result<TableSnapshotArgument, ()> {
    let bytes = argument.as_encoded_bytes();
    let separator = bytes.iter().position(|byte| *byte == b'=').ok_or(())?;
    let name = std::str::from_utf8(&bytes[..separator]).map_err(|_| ())?;
    let path = &bytes[separator + 1..];
    if !valid_table_name(name) || path.is_empty() {
        return Err(());
    }

    // OsStr's encoded form is self-synchronizing, and ASCII '=' is a valid
    // boundary, so the suffix remains an encoded substring of `argument`.
    let path = unsafe { OsStr::from_encoded_bytes_unchecked(path) };
    Ok(TableSnapshotArgument {
        name: name.to_owned(),
        path: PathBuf::from(path),
    })
}

fn valid_table_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}
