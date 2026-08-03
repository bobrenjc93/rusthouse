use rusthouse::output::{OutputFormat, write_results};
use rusthouse::{execute_batch, read_sql_input};
use std::env;
use std::ffi::OsString;
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
    let action = match parse_args(env::args_os().skip(1)) {
        Ok(action) => action,
        Err(error) => {
            report_error(&format!(
                "{error}\nTry 'rusthouse --help' for more information."
            ));
            return ExitCode::from(2);
        }
    };

    let CliAction::Execute { format } = action else {
        let stdout = io::stdout();
        return match write_help(stdout.lock()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                report_error(&format!("failed to write help: {error}"));
                ExitCode::from(1)
            }
        };
    };

    let input = match read_sql_input(io::stdin().lock()) {
        Ok(input) => input,
        Err(error) => {
            report_error(&error.to_string());
            return ExitCode::from(1);
        }
    };
    let results = match execute_batch(&input) {
        Ok(results) => results,
        Err(error) => {
            report_error(&error.to_string());
            return ExitCode::from(1);
        }
    };

    let stdout = io::stdout();
    let mut output = io::BufWriter::new(stdout.lock());
    if let Err(error) = write_results(&results, format, &mut output).and_then(|()| output.flush()) {
        report_error(&format!("failed to write query results: {error}"));
        return ExitCode::from(1);
    }

    ExitCode::SUCCESS
}

fn parse_args(arguments: impl IntoIterator<Item = OsString>) -> Result<CliAction, String> {
    let mut arguments = arguments.into_iter();
    let mut format = OutputFormat::Table;
    let mut format_seen = false;

    while let Some(argument) = arguments.next() {
        let argument = argument
            .into_string()
            .map_err(|_| "argument is not valid UTF-8".to_owned())?;
        match argument.as_str() {
            "-h" | "--help" => return Ok(CliAction::Help),
            "--format" => {
                if format_seen {
                    return Err("--format may only be specified once".to_owned());
                }
                let value = arguments
                    .next()
                    .ok_or_else(|| "--format requires a value".to_owned())?;
                let value = value
                    .into_string()
                    .map_err(|_| "--format value is not valid UTF-8".to_owned())?;
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
            value => {
                return Err(format!(
                    "unrecognized argument `{}`",
                    escape_for_diagnostic(value)
                ));
            }
        }
    }

    Ok(CliAction::Execute { format })
}

fn parse_format(value: &str) -> Result<OutputFormat, String> {
    match value {
        "table" => Ok(OutputFormat::Table),
        "csv" => Ok(OutputFormat::Csv),
        _ => Err(format!(
            "unsupported output format `{}`; expected table or csv",
            escape_for_diagnostic(value)
        )),
    }
}

fn escape_for_diagnostic(value: &str) -> String {
    value.chars().flat_map(char::escape_default).collect()
}

fn write_help<W: Write>(mut writer: W) -> io::Result<()> {
    writer.write_all(HELP.as_bytes())?;
    writer.flush()
}

fn report_error(message: &str) {
    let stderr = io::stderr();
    let mut stderr = stderr.lock();
    let _ = writeln!(stderr, "rusthouse: {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed pipe"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn parses_csv_format() {
        let action = parse_args([OsString::from("--format=csv")]).unwrap();
        assert!(matches!(
            action,
            CliAction::Execute {
                format: OutputFormat::Csv
            }
        ));
    }

    #[test]
    fn rejects_unknown_arguments() {
        assert!(parse_args([OsString::from("query.sql")]).is_err());
    }

    #[test]
    fn help_write_failures_are_returned() {
        let error = write_help(FailingWriter).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }
}
