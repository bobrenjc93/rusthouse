//! Command-line front end for RustHouse.

use std::env;
use std::io::{self, Read};
use std::process::ExitCode;

use rusthouse::format::{OutputFormat, render_with_limit};
use rusthouse::{Database, Error, QueryResult, Resource, StatementResult};

const HELP: &str = "\
RustHouse - an in-memory columnar SQL engine

USAGE:
    rusthouse [OPTIONS]

OPTIONS:
    -e, --execute <SQL>       Execute SQL supplied as an argument
    -f, --format <FORMAT>     Output format: table (default), csv, or json
    -w, --workers <COUNT>     Maximum parallel scan workers
    -h, --help                Print this help

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
    let Some(config) = parse_arguments(env::args().skip(1))? else {
        print!("{HELP}");
        return Ok(());
    };

    let mut database = config
        .workers
        .map_or_else(|| Ok(Database::new()), Database::with_worker_count)
        .map_err(|error| error.to_string())?;
    let max_input_bytes = database.limits().max_input_bytes;
    let max_rendered_bytes = database.limits().max_rendered_bytes;
    let sql = if let Some(sql) = config.execute {
        sql
    } else {
        read_sql_bounded(io::stdin(), max_input_bytes)?
    };

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
    print!(
        "{}",
        render_query_results_bounded(&queries, config.format, max_rendered_bytes)
            .map_err(|error| error.to_string())?
    );
    Ok(())
}

fn read_sql_bounded(reader: impl Read, max_bytes: usize) -> Result<String, String> {
    let read_limit = u64::try_from(max_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut bytes = Vec::new();
    reader
        .take(read_limit)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read SQL from stdin: {error}"))?;
    if bytes.len() > max_bytes {
        return Err(Error::ResourceLimitExceeded {
            resource: Resource::InputBytes,
            limit: max_bytes,
            actual: bytes.len(),
        }
        .to_string());
    }
    String::from_utf8(bytes).map_err(|error| format!("SQL input is not valid UTF-8: {error}"))
}

#[cfg(test)]
fn render_query_results(results: &[QueryResult], format: OutputFormat) -> String {
    render_query_results_bounded(results, format, usize::MAX)
        .expect("unbounded rendering cannot exceed its limit")
}

fn render_query_results_bounded(
    results: &[QueryResult],
    format: OutputFormat,
    max_bytes: usize,
) -> rusthouse::Result<String> {
    if format == OutputFormat::Json {
        let mut output = String::new();
        append_bounded(&mut output, "{\"results\":[", max_bytes)?;
        for (index, result) in results.iter().enumerate() {
            if index > 0 {
                append_bounded(&mut output, ",", max_bytes)?;
            }
            let rendered = render_with_limit(result, format, max_bytes)?;
            append_bounded(&mut output, &rendered, max_bytes)?;
        }
        append_bounded(&mut output, "]}\n", max_bytes)?;
        return Ok(output);
    }

    let mut output = String::new();
    for (index, result) in results.iter().enumerate() {
        if index > 0 {
            append_bounded(&mut output, "\n", max_bytes)?;
        }
        let rendered = render_with_limit(result, format, max_bytes)?;
        append_bounded(&mut output, &rendered, max_bytes)?;
        if !rendered.ends_with('\n') {
            append_bounded(&mut output, "\n", max_bytes)?;
        }
    }
    Ok(output)
}

fn append_bounded(output: &mut String, value: &str, limit: usize) -> rusthouse::Result<()> {
    let actual = output.len().saturating_add(value.len());
    if actual > limit {
        return Err(Error::ResourceLimitExceeded {
            resource: Resource::RenderedBytes,
            limit,
            actual,
        });
    }
    output.push_str(value);
    Ok(())
}

#[derive(Debug)]
struct Config {
    execute: Option<String>,
    format: OutputFormat,
    workers: Option<usize>,
}

fn parse_arguments(arguments: impl Iterator<Item = String>) -> Result<Option<Config>, String> {
    let mut execute = None;
    let mut format = OutputFormat::Table;
    let mut workers = None;
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
            "-w" | "--workers" => {
                if workers.is_some() {
                    return Err("--workers may only be supplied once".to_owned());
                }
                let value = arguments
                    .next()
                    .ok_or_else(|| format!("{argument} requires a worker count"))?;
                workers = Some(parse_worker_count(&value)?);
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
            _ if argument.starts_with("--workers=") => {
                if workers.is_some() {
                    return Err("--workers may only be supplied once".to_owned());
                }
                workers = Some(parse_worker_count(&argument["--workers=".len()..])?);
            }
            _ => return Err(format!("unknown argument '{argument}'; try --help")),
        }
    }

    Ok(Some(Config {
        execute,
        format,
        workers,
    }))
}

fn parse_worker_count(value: &str) -> Result<usize, String> {
    let workers = value
        .parse::<usize>()
        .map_err(|_| format!("invalid worker count '{value}'; expected a positive integer"))?;
    if workers == 0 {
        return Err("invalid worker count '0'; expected a positive integer".to_owned());
    }
    Ok(workers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusthouse::{DataType, ResultColumn, Value};
    use std::io::Cursor;

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
        assert_eq!(config.workers, None);
    }

    #[test]
    fn rejects_unknown_formats() {
        let error = parse_arguments(["--format", "xml"].into_iter().map(str::to_owned))
            .expect_err("unknown format");
        assert!(error.contains("table, csv, or json"));
    }

    #[test]
    fn parses_and_validates_worker_counts() {
        let config = parse_arguments(["--workers=4"].into_iter().map(str::to_owned))
            .expect("valid arguments")
            .expect("not help");
        assert_eq!(config.workers, Some(4));

        for value in ["0", "many"] {
            let error = parse_arguments(["--workers", value].into_iter().map(str::to_owned))
                .expect_err("worker count is invalid");
            assert!(error.contains("expected a positive integer"));
        }
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
    fn stdin_reader_stops_one_byte_past_the_input_limit() {
        let mut input = Cursor::new(b"SELECT 1234567890".to_vec());
        let error = read_sql_bounded(&mut input, 6).expect_err("seventh byte exceeds limit");

        assert!(error.contains("limit 6, observed 7"));
        assert_eq!(input.position(), 7);

        let exact = read_sql_bounded(Cursor::new(b"SELECT"), 6).expect("exact limit succeeds");
        assert_eq!(exact, "SELECT");
    }
}
