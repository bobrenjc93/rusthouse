use std::env;
use std::io::{self, Read};
use std::process::ExitCode;

use rusthouse::format::{OutputFormat, render_results};
use rusthouse::{Database, StatementResult};

const HELP: &str = "\
RustHouse - an in-memory columnar SQL engine

USAGE:
    rusthouse [OPTIONS]
    rusthouse serve --listen <ADDRESS>

OPTIONS:
    -e, --execute <SQL>       Execute SQL supplied as an argument
    -f, --format <FORMAT>     Output format: table (default), csv, or json
    -h, --help                Print this help

SERVER OPTIONS:
    --listen <ADDRESS>        TCP address to listen on, for example 127.0.0.1:8080

With no --execute option, SQL is read to EOF from standard input.
Command acknowledgements are written to stderr; query data is written to stdout.
JSON output is an object containing a results array, one entry per SELECT.
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
    let Some(command) = parse_arguments(env::args().skip(1))? else {
        print!("{HELP}");
        return Ok(());
    };

    match command {
        Command::Query(config) => run_query(config),
        Command::Serve { listen } => rusthouse::server::serve(&listen)
            .map_err(|error| format!("HTTP server failed: {error}")),
    }
}

fn run_query(config: QueryConfig) -> Result<(), String> {
    let sql = if let Some(sql) = config.execute {
        sql
    } else {
        let mut sql = String::new();
        io::stdin()
            .read_to_string(&mut sql)
            .map_err(|error| format!("could not read SQL from stdin: {error}"))?;
        sql
    };

    let mut database = Database::new();
    let results = database.execute(&sql).map_err(|error| error.to_string())?;
    let mut queries = Vec::new();
    for result in results {
        match result {
            StatementResult::Command { tag, affected_rows } => {
                if tag == "INSERT" {
                    eprintln!("{tag} {affected_rows}");
                } else {
                    eprintln!("{tag}");
                }
            }
            StatementResult::Query(result) => queries.push(result),
        }
    }
    print!("{}", render_results(&queries, config.format));
    Ok(())
}

#[derive(Debug)]
enum Command {
    Query(QueryConfig),
    Serve { listen: String },
}

#[derive(Debug)]
struct QueryConfig {
    execute: Option<String>,
    format: OutputFormat,
}

fn parse_arguments(arguments: impl Iterator<Item = String>) -> Result<Option<Command>, String> {
    let mut arguments = arguments.peekable();
    if arguments.peek().is_some_and(|argument| argument == "serve") {
        arguments.next();
        if arguments
            .peek()
            .is_some_and(|argument| argument == "-h" || argument == "--help")
        {
            return Ok(None);
        }
        return parse_serve_arguments(arguments).map(Some);
    }

    let mut execute = None;
    let mut format = OutputFormat::Table;

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

    Ok(Some(Command::Query(QueryConfig { execute, format })))
}

fn parse_serve_arguments(mut arguments: impl Iterator<Item = String>) -> Result<Command, String> {
    let mut listen = None;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--listen" => {
                if listen.is_some() {
                    return Err("--listen may only be supplied once".to_owned());
                }
                listen = Some(
                    arguments
                        .next()
                        .ok_or_else(|| "--listen requires an address".to_owned())?,
                );
            }
            _ if argument.starts_with("--listen=") => {
                if listen.is_some() {
                    return Err("--listen may only be supplied once".to_owned());
                }
                listen = Some(argument["--listen=".len()..].to_owned());
            }
            _ => return Err(format!("unknown server argument '{argument}'; try --help")),
        }
    }
    let listen = listen.ok_or_else(|| "serve requires --listen <ADDRESS>".to_owned())?;
    if listen.is_empty() {
        return Err("--listen requires a non-empty address".to_owned());
    }
    Ok(Command::Serve { listen })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusthouse::{DataType, QueryResult, ResultColumn, Value};

    #[test]
    fn parses_equals_style_options() {
        let command = parse_arguments(
            ["--format=json", "--execute=SELECT * FROM t"]
                .into_iter()
                .map(str::to_owned),
        )
        .expect("valid arguments")
        .expect("not help");
        let Command::Query(config) = command else {
            panic!("expected query command");
        };
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
            render_results(&[result.clone(), result], OutputFormat::Json),
            "{\"results\":[{\"columns\":[{\"name\":\"n\",\"type\":\"Int64\"}],\"rows\":[[1]]},{\"columns\":[{\"name\":\"n\",\"type\":\"Int64\"}],\"rows\":[[1]]}]}\n"
        );
    }

    #[test]
    fn parses_serve_listen_address() {
        let command = parse_arguments(
            ["serve", "--listen", "127.0.0.1:8080"]
                .into_iter()
                .map(str::to_owned),
        )
        .expect("valid arguments")
        .expect("not help");
        let Command::Serve { listen } = command else {
            panic!("expected serve command");
        };
        assert_eq!(listen, "127.0.0.1:8080");
    }

    #[test]
    fn serve_requires_listen_address() {
        let error = parse_arguments(["serve"].into_iter().map(str::to_owned))
            .expect_err("missing listen address");
        assert!(error.contains("requires --listen"));
    }
}
