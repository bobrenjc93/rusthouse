use std::env;
use std::ffi::OsString;
use std::io::{self, Read};
use std::process::ExitCode;

const HELP: &str = "RustHouse - a compact analytical database

Usage: rusthouse --format csv

Options:
      --format <FORMAT>  Output format [possible values: csv]
  -h, --help             Print help
";

enum Command {
    Help,
    Run,
}

fn main() -> ExitCode {
    match parse_args(env::args_os().skip(1)) {
        Ok(Command::Help) => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
        Ok(Command::Run) => match execute() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("error: {error}");
                ExitCode::from(1)
            }
        },
        Err(error) => {
            eprintln!("error: {error}\n\nFor more information, try '--help'.");
            ExitCode::from(2)
        }
    }
}

fn parse_args(arguments: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let mut arguments = arguments.into_iter();
    let mut format_seen = false;

    while let Some(argument) = arguments.next() {
        let argument = argument
            .into_string()
            .map_err(|_| "arguments must be valid UTF-8".to_owned())?;

        match argument.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "--format" => {
                if format_seen {
                    return Err("--format may only be specified once".to_owned());
                }
                let value = arguments
                    .next()
                    .ok_or_else(|| "--format requires a value".to_owned())?
                    .into_string()
                    .map_err(|_| "the --format value must be valid UTF-8".to_owned())?;
                validate_format(&value)?;
                format_seen = true;
            }
            _ if argument.starts_with("--format=") => {
                if format_seen {
                    return Err("--format may only be specified once".to_owned());
                }
                validate_format(&argument["--format=".len()..])?;
                format_seen = true;
            }
            _ if argument.starts_with('-') => {
                return Err(format!("unrecognized option '{argument}'"));
            }
            _ => return Err(format!("unexpected argument '{argument}'")),
        }
    }

    if !format_seen {
        return Err("the required --format csv option is missing".to_owned());
    }
    Ok(Command::Run)
}

fn validate_format(format: &str) -> Result<(), String> {
    if format == "csv" {
        Ok(())
    } else if format.is_empty() {
        Err("--format requires a value".to_owned())
    } else {
        Err(format!(
            "unsupported output format '{format}'; expected 'csv'"
        ))
    }
}

fn execute() -> Result<(), String> {
    let mut sql = String::new();
    io::stdin()
        .read_to_string(&mut sql)
        .map_err(|error| format!("failed to read SQL from stdin: {error}"))?;

    let results = rusthouse::parse_sql_batch(&sql).map_err(|error| error.to_string())?;
    let stdout = io::stdout();
    let mut output = stdout.lock();
    rusthouse::write_csv(&results, &mut output)
        .map_err(|error| format!("failed to write CSV to stdout: {error}"))
}
