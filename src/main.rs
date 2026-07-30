use std::env;
use std::fs::File;
use std::io::BufReader;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use rusthouse::format::{OutputFormat, render};
use rusthouse::{Database, QueryResult, StatementResult};

const HELP: &str = "\
RustHouse - an in-memory columnar SQL engine

USAGE:
    rusthouse [OPTIONS]

OPTIONS:
    -e, --execute <SQL>       Execute SQL supplied as an argument
    -i, --input <PATH>        Read CSV input from PATH instead of standard input
    -f, --format <FORMAT>     Output format: table (default), csv, or json
    -h, --help                Print this help

With no --execute option, SQL is read to EOF from standard input.
With --execute, INSERT ... FORMAT CSV input is read from stdin or --input PATH.
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
    let Some(config) = parse_arguments(env::args().skip(1))? else {
        print!("{HELP}");
        return Ok(());
    };

    let sql_from_execute = config.execute.is_some();
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
    let results = if sql_from_execute {
        if let Some(path) = config.input {
            let file = File::open(&path)
                .map_err(|error| format!("could not open input {}: {error}", path.display()))?;
            database.execute_with_input(&sql, BufReader::new(file))
        } else {
            let stdin = io::stdin();
            database.execute_with_input(&sql, stdin.lock())
        }
    } else {
        database.execute(&sql)
    }
    .map_err(|error| error.to_string())?;
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
    print!("{}", render_query_results(&queries, config.format));
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
    input: Option<PathBuf>,
    format: OutputFormat,
}

fn parse_arguments(arguments: impl Iterator<Item = String>) -> Result<Option<Config>, String> {
    let mut execute = None;
    let mut input = None;
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
            "-i" | "--input" => {
                if input.is_some() {
                    return Err("--input may only be supplied once".to_owned());
                }
                input = Some(PathBuf::from(
                    arguments
                        .next()
                        .ok_or_else(|| format!("{argument} requires a path"))?,
                ));
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
            _ if argument.starts_with("--input=") => {
                if input.is_some() {
                    return Err("--input may only be supplied once".to_owned());
                }
                input = Some(PathBuf::from(&argument["--input=".len()..]));
            }
            _ => return Err(format!("unknown argument '{argument}'; try --help")),
        }
    }

    if input.is_some() && execute.is_none() {
        return Err("--input requires --execute because stdin otherwise contains SQL".to_owned());
    }

    Ok(Some(Config {
        execute,
        input,
        format,
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
        assert_eq!(config.input, None);
    }

    #[test]
    fn input_requires_execute() {
        let error = parse_arguments(["--input=data.csv"].into_iter().map(str::to_owned))
            .expect_err("input cannot share stdin with SQL");
        assert!(error.contains("requires --execute"));
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
