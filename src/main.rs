use std::env;
use std::fmt;
use std::io::{self, BufWriter, Read, Write};
use std::process::ExitCode;

use rusthouse::format::{OutputFormat, render};
use rusthouse::{Database, QueryResult, StatementResult};

const HELP: &str = "\
RustHouse - an in-memory columnar SQL engine

USAGE:
    rusthouse [OPTIONS]

OPTIONS:
    -e, --execute <SQL>       Execute SQL supplied as an argument
    -f, --format <FORMAT>     Output format: table (default), csv, or json
    -h, --help                Print this help

With no --execute option, SQL is read to EOF from standard input.
Command acknowledgements are written to stderr; query data is written to stdout.
JSON output is an object containing a results array, one entry per SELECT.
";

fn main() -> ExitCode {
    let stdout = io::stdout();
    let stderr = io::stderr();
    let mut stdout = BufWriter::new(stdout.lock());
    let mut stderr = BufWriter::new(stderr.lock());

    let result = run(&mut stdout, &mut stderr).and_then(|()| {
        stderr.flush().map_err(CliError::Stderr)?;
        stdout.flush().map_err(CliError::Stdout)
    });

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(CliError::Stdout(error)) if error.kind() == io::ErrorKind::BrokenPipe => {
            let _ = stderr.flush();
            ExitCode::SUCCESS
        }
        Err(error) => {
            let _ = write_error(&mut stderr, &error);
            ExitCode::FAILURE
        }
    }
}

fn write_error(stderr: &mut impl Write, error: &CliError) -> io::Result<()> {
    writeln!(stderr, "error: {error}")?;
    stderr.flush()
}

fn run(stdout: &mut impl Write, stderr: &mut impl Write) -> Result<(), CliError> {
    let Some(config) = parse_arguments(env::args().skip(1)).map_err(CliError::Message)? else {
        stdout
            .write_all(HELP.as_bytes())
            .map_err(CliError::Stdout)?;
        return Ok(());
    };

    let sql = if let Some(sql) = config.execute {
        sql
    } else {
        let mut sql = String::new();
        io::stdin().read_to_string(&mut sql).map_err(|error| {
            CliError::Message(format!("could not read SQL from stdin: {error}"))
        })?;
        sql
    };

    let mut database = Database::new();
    let results = database
        .execute(&sql)
        .map_err(|error| CliError::Message(error.to_string()))?;
    let mut queries = Vec::new();
    for result in results {
        match result {
            StatementResult::Command { tag, affected_rows } => {
                if tag == "INSERT" {
                    writeln!(stderr, "{tag} {affected_rows}").map_err(CliError::Stderr)?;
                } else {
                    writeln!(stderr, "{tag}").map_err(CliError::Stderr)?;
                }
            }
            StatementResult::Query(result) => queries.push(result),
        }
    }
    stdout
        .write_all(render_query_results(&queries, config.format).as_bytes())
        .map_err(CliError::Stdout)?;
    Ok(())
}

#[derive(Debug)]
enum CliError {
    Message(String),
    Stdout(io::Error),
    Stderr(io::Error),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(message) => formatter.write_str(message),
            Self::Stdout(error) => write!(formatter, "could not write to stdout: {error}"),
            Self::Stderr(error) => write!(formatter, "could not write to stderr: {error}"),
        }
    }
}

fn render_query_results(results: &[QueryResult], format: OutputFormat) -> String {
    if format == OutputFormat::Json {
        let rendered = results
            .iter()
            .map(|result| render(result, format))
            .collect::<Vec<_>>()
            .join(",");
        return format!("{{\"results\":[{rendered}]}}\n");
    }

    let mut output = String::new();
    for (index, result) in results.iter().enumerate() {
        if index > 0 {
            output.push('\n');
        }
        let rendered = render(result, format);
        output.push_str(&rendered);
        if !rendered.ends_with('\n') {
            output.push('\n');
        }
    }
    output
}

#[derive(Debug)]
struct Config {
    execute: Option<String>,
    format: OutputFormat,
}

fn parse_arguments(arguments: impl Iterator<Item = String>) -> Result<Option<Config>, String> {
    let mut execute = None;
    let mut format = OutputFormat::Table;
    let mut arguments = arguments.peekable();

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "-h" | "--help" => return Ok(None),
            "-e" | "--execute" => {
                if execute.is_some() {
                    return Err("--execute may only be supplied once".to_owned());
                }
                execute = Some(
                    arguments
                        .next()
                        .ok_or_else(|| format!("{argument} requires a SQL argument"))?,
                );
            }
            "-f" | "--format" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| format!("{argument} requires a format"))?;
                format = OutputFormat::parse(&value).ok_or_else(|| {
                    format!("unknown output format '{value}'; expected table, csv, or json")
                })?;
            }
            _ if argument.starts_with("--execute=") => {
                if execute.is_some() {
                    return Err("--execute may only be supplied once".to_owned());
                }
                execute = Some(argument["--execute=".len()..].to_owned());
            }
            _ if argument.starts_with("--format=") => {
                let value = &argument["--format=".len()..];
                format = OutputFormat::parse(value).ok_or_else(|| {
                    format!("unknown output format '{value}'; expected table, csv, or json")
                })?;
            }
            _ => return Err(format!("unknown argument '{argument}'; try --help")),
        }
    }

    Ok(Some(Config { execute, format }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusthouse::{DataType, ResultColumn, Value};

    #[test]
    fn parses_equals_style_options() {
        let config = parse_arguments(
            ["--format=json", "--execute=SELECT * FROM t"]
                .into_iter()
                .map(str::to_owned),
        )
        .expect("valid arguments")
        .expect("not help");
        assert_eq!(config.format, OutputFormat::Json);
        assert_eq!(config.execute.as_deref(), Some("SELECT * FROM t"));
    }

    #[test]
    fn rejects_unknown_formats() {
        let error = parse_arguments(["--format", "xml"].into_iter().map(str::to_owned))
            .expect_err("unknown format");
        assert!(error.contains("table, csv, or json"));
    }

    #[test]
    fn wraps_multiple_json_query_results_in_one_document() {
        let result = QueryResult {
            columns: vec![ResultColumn {
                name: "n".to_owned(),
                data_type: DataType::Int64,
            }],
            rows: vec![vec![Value::Int64(1)]],
        };

        assert_eq!(
            render_query_results(&[result.clone(), result], OutputFormat::Json),
            "{\"results\":[{\"columns\":[{\"name\":\"n\",\"type\":\"Int64\"}],\"rows\":[[1]]},{\"columns\":[{\"name\":\"n\",\"type\":\"Int64\"}],\"rows\":[[1]]}]}\n"
        );
    }
}
