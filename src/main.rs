use std::env;
use std::io::{self, IsTerminal, Read, Write};
use std::process::ExitCode;

use rusthouse::format::{OutputFormat, render};
use rusthouse::{Database, QueryResult, StatementResult};

mod shell;

const HELP: &str = "\
RustHouse - an in-memory columnar SQL engine

USAGE:
    rusthouse [OPTIONS]

OPTIONS:
    -e, --execute <SQL>       Execute SQL supplied as an argument
    -f, --format <FORMAT>     Output format: table (default), csv, or json
    -i, --interactive         Run the interactive SQL shell
    -h, --help                Print this help

With no --execute option, terminal input starts the interactive shell. Piped
input is read to EOF as one batch unless --interactive is supplied.
Command acknowledgements are written to stderr; query data is written to stdout.
JSON output is an object containing a results array, one entry per SELECT.

INTERACTIVE COMMANDS:
    \\q                       Quit
    \\format [FORMAT]         Show or set the output format
    \\read <PATH>             Execute SQL from a file
";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let Some(config) = parse_arguments(env::args().skip(1))? else {
        print!("{HELP}");
        return Ok(());
    };

    if let Some(sql) = config.execute {
        return run_batch(&sql, config.format);
    }

    let stdin = io::stdin();
    if config.interactive || stdin.is_terminal() {
        let stdout = io::stdout();
        let stderr = io::stderr();
        return shell::run(stdin.lock(), stdout.lock(), stderr.lock(), config.format);
    }

    let mut sql = String::new();
    stdin
        .lock()
        .read_to_string(&mut sql)
        .map_err(|error| format!("could not read SQL from stdin: {error}"))?;
    run_batch(&sql, config.format)
}

fn run_batch(sql: &str, format: OutputFormat) -> Result<(), String> {
    let mut database = Database::new();
    let results = database.execute(sql).map_err(|error| error.to_string())?;
    let stdout = io::stdout();
    let stderr = io::stderr();
    emit_results(
        results,
        format,
        true,
        &mut stdout.lock(),
        &mut stderr.lock(),
    )
    .map_err(|error| format!("could not write output: {error}"))
}

fn emit_results(
    results: Vec<StatementResult>,
    format: OutputFormat,
    render_empty_queries: bool,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> io::Result<()> {
    let mut queries = Vec::new();
    for result in results {
        match result {
            StatementResult::Command { tag, affected_rows } => {
                if tag == "INSERT" {
                    writeln!(stderr, "{tag} {affected_rows}")?;
                } else {
                    writeln!(stderr, "{tag}")?;
                }
            }
            StatementResult::Query(result) => queries.push(result),
        }
    }
    if render_empty_queries || !queries.is_empty() {
        stdout.write_all(render_query_results(&queries, format).as_bytes())?;
        stdout.flush()?;
    }
    Ok(())
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
    interactive: bool,
}

fn parse_arguments(arguments: impl Iterator<Item = String>) -> Result<Option<Config>, String> {
    let mut execute = None;
    let mut format = OutputFormat::Table;
    let mut interactive = false;
    let mut arguments = arguments.peekable();

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "-h" | "--help" => return Ok(None),
            "-i" | "--interactive" => {
                if interactive {
                    return Err("--interactive may only be supplied once".to_owned());
                }
                interactive = true;
            }
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

    if interactive && execute.is_some() {
        return Err("--interactive cannot be combined with --execute".to_owned());
    }

    Ok(Some(Config {
        execute,
        format,
        interactive,
    }))
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
        assert!(!config.interactive);
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

    #[test]
    fn interactive_and_execute_are_mutually_exclusive() {
        let error = parse_arguments(
            ["--interactive", "--execute", "SELECT * FROM t"]
                .into_iter()
                .map(str::to_owned),
        )
        .expect_err("conflicting input modes");

        assert!(error.contains("cannot be combined"));
    }
}
