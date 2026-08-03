use rusthouse::output::{OutputFormat, write_results};
use rusthouse::{execute_batch, read_sql_input};
use std::env;
use std::io::{self, Write};
use std::process::ExitCode;

const HELP: &str = "RustHouse - execute literal SELECT statements

Usage: rusthouse [OPTIONS]

Reads a SQL batch from standard input and writes each result to standard output.

Options:
      --format <FORMAT>  Output format: table or csv [default: table]
  -h, --help             Print help
";

enum CliAction {
    Help,
    Execute { format: OutputFormat },
}

fn main() -> ExitCode {
    let action = match parse_args(env::args().skip(1)) {
        Ok(action) => action,
        Err(error) => {
            eprintln!("rusthouse: {error}\nTry 'rusthouse --help' for more information.");
            return ExitCode::from(2);
        }
    };

    let CliAction::Execute { format } = action else {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    };

    let input = match read_sql_input(io::stdin().lock()) {
        Ok(input) => input,
        Err(error) => {
            eprintln!("rusthouse: {error}");
            return ExitCode::from(1);
        }
    };
    let results = match execute_batch(&input) {
        Ok(results) => results,
        Err(error) => {
            eprintln!("rusthouse: {error}");
            return ExitCode::from(1);
        }
    };

    let stdout = io::stdout();
    let mut output = io::BufWriter::new(stdout.lock());
    if let Err(error) = write_results(&results, format, &mut output).and_then(|()| output.flush()) {
        eprintln!("rusthouse: failed to write query results: {error}");
        return ExitCode::from(1);
    }

    ExitCode::SUCCESS
}

fn parse_args(arguments: impl IntoIterator<Item = String>) -> Result<CliAction, String> {
    let mut arguments = arguments.into_iter();
    let mut format = OutputFormat::Table;
    let mut format_seen = false;

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "-h" | "--help" => return Ok(CliAction::Help),
            "--format" => {
                if format_seen {
                    return Err("--format may only be specified once".to_owned());
                }
                let value = arguments
                    .next()
                    .ok_or_else(|| "--format requires a value".to_owned())?;
                format = parse_format(&value)?;
                format_seen = true;
            }
            value if value.starts_with("--format=") => {
                if format_seen {
                    return Err("--format may only be specified once".to_owned());
                }
                format = parse_format(&value["--format=".len()..])?;
                format_seen = true;
            }
            value => return Err(format!("unrecognized argument `{value}`")),
        }
    }

    Ok(CliAction::Execute { format })
}

fn parse_format(value: &str) -> Result<OutputFormat, String> {
    match value {
        "table" => Ok(OutputFormat::Table),
        "csv" => Ok(OutputFormat::Csv),
        _ => Err(format!(
            "unsupported output format `{value}`; expected table or csv"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_csv_format() {
        let action = parse_args(["--format=csv".to_owned()]).unwrap();
        assert!(matches!(
            action,
            CliAction::Execute {
                format: OutputFormat::Csv
            }
        ));
    }

    #[test]
    fn rejects_unknown_arguments() {
        assert!(parse_args(["query.sql".to_owned()]).is_err());
    }
}
