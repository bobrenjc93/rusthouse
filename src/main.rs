use std::env;
use std::io::{self, Read, Write};
use std::process::ExitCode;

use rusthouse::format::{OutputFormat, write as write_result};
use rusthouse::{Database, QueryResult, StatementResult};

const HELP: &str = "\
RustHouse - an in-memory columnar SQL engine

USAGE:
    rusthouse [OPTIONS]

OPTIONS:
    -e, --execute <SQL>       Execute SQL supplied as an argument
    -f, --format <FORMAT>     Output format: table, csv, json, or JSONEachRow
    -h, --help                Print this help

With no --execute option, SQL is read to EOF from standard input.
Command acknowledgements are written to stderr; query data is written to stdout.
JSON output is an object containing a results array, one entry per SELECT.
JSONEachRow output writes one object per result row.
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
                if matches!(tag, "INSERT" | "COPY") {
                    eprintln!("{tag} {affected_rows}");
                } else {
                    eprintln!("{tag}");
                }
            }
            StatementResult::Query(result) => queries.push(result),
        }
    }
    let stdout = io::stdout();
    write_query_results(&queries, config.format, &mut stdout.lock())
        .map_err(|error| format!("could not write query output: {error}"))?;
    Ok(())
}

fn write_query_results<W: Write>(
    results: &[QueryResult],
    format: OutputFormat,
    writer: &mut W,
) -> io::Result<()> {
    if format == OutputFormat::Json {
        writer.write_all(b"{\"results\":[")?;
        for (index, result) in results.iter().enumerate() {
            if index > 0 {
                writer.write_all(b",")?;
            }
            write_result(result, format, writer)?;
        }
        return writer.write_all(b"]}\n");
    }

    for (index, result) in results.iter().enumerate() {
        if index > 0 && format != OutputFormat::JsonEachRow {
            writer.write_all(b"\n")?;
        }
        write_result(result, format, writer)?;
        if format == OutputFormat::Table {
            writer.write_all(b"\n")?;
        }
    }
    Ok(())
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
                    format!(
                        "unknown output format '{value}'; expected table, csv, json, or JSONEachRow"
                    )
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
                    format!(
                        "unknown output format '{value}'; expected table, csv, json, or JSONEachRow"
                    )
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
        assert!(error.contains("table, csv, json, or JSONEachRow"));
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
            {
                let mut output = Vec::new();
                write_query_results(&[result.clone(), result], OutputFormat::Json, &mut output)
                    .expect("write results");
                String::from_utf8(output).expect("UTF-8")
            },
            "{\"results\":[{\"columns\":[{\"name\":\"n\",\"type\":\"Int64\"}],\"rows\":[[1]]},{\"columns\":[{\"name\":\"n\",\"type\":\"Int64\"}],\"rows\":[[1]]}]}\n"
        );
    }
}
