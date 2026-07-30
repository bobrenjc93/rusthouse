use std::env;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use rusthouse::format::{OutputFormat, render};
use rusthouse::parquet::write_parquet;
use rusthouse::{Database, QueryResult, StatementResult};

const HELP: &str = "\
RustHouse - an in-memory columnar SQL engine

USAGE:
    rusthouse [OPTIONS]

OPTIONS:
    -e, --execute <SQL>       Execute SQL supplied as an argument
    -f, --format <FORMAT>     Output format: table (default), csv, json, or parquet
        --output <PATH>       Parquet output path (required for parquet)
    -h, --help                Print this help

With no --execute option, SQL is read to EOF from standard input.
Command acknowledgements are written to stderr; text query data is written to stdout.
Parquet query data is written atomically to --output PATH.
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
    match config.format {
        CliOutputFormat::Text(format) => {
            print!("{}", render_query_results(&queries, format));
            Ok(())
        }
        CliOutputFormat::Parquet => {
            if queries.len() != 1 {
                return Err(format!(
                    "Parquet output requires exactly one SELECT, but the batch produced {}",
                    queries.len()
                ));
            }
            write_parquet(
                &queries[0],
                config
                    .output
                    .as_deref()
                    .expect("Parquet output path was validated"),
            )
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
    format: CliOutputFormat,
    output: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliOutputFormat {
    Text(OutputFormat),
    Parquet,
}

impl CliOutputFormat {
    fn parse(value: &str) -> Option<Self> {
        if value.eq_ignore_ascii_case("parquet") {
            Some(Self::Parquet)
        } else {
            OutputFormat::parse(value).map(Self::Text)
        }
    }
}

fn parse_arguments(arguments: impl Iterator<Item = String>) -> Result<Option<Config>, String> {
    let mut execute = None;
    let mut format = CliOutputFormat::Text(OutputFormat::Table);
    let mut output = None;
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
                format = CliOutputFormat::parse(&value).ok_or_else(|| {
                    format!(
                        "unknown output format '{value}'; expected table, csv, json, or parquet"
                    )
                })?;
            }
            "--output" => {
                if output.is_some() {
                    return Err("--output may only be supplied once".to_owned());
                }
                let value = arguments
                    .next()
                    .ok_or_else(|| "--output requires a path".to_owned())?;
                if value.is_empty() {
                    return Err("--output requires a non-empty path".to_owned());
                }
                output = Some(PathBuf::from(value));
            }
            _ if argument.starts_with("--execute=") => {
                if execute.is_some() {
                    return Err("--execute may only be supplied once".to_owned());
                }
                execute = Some(argument["--execute=".len()..].to_owned());
            }
            _ if argument.starts_with("--format=") => {
                let value = &argument["--format=".len()..];
                format = CliOutputFormat::parse(value).ok_or_else(|| {
                    format!(
                        "unknown output format '{value}'; expected table, csv, json, or parquet"
                    )
                })?;
            }
            _ if argument.starts_with("--output=") => {
                if output.is_some() {
                    return Err("--output may only be supplied once".to_owned());
                }
                let value = &argument["--output=".len()..];
                if value.is_empty() {
                    return Err("--output requires a non-empty path".to_owned());
                }
                output = Some(PathBuf::from(value));
            }
            _ => return Err(format!("unknown argument '{argument}'; try --help")),
        }
    }

    match (format, output.as_ref()) {
        (CliOutputFormat::Parquet, None) => {
            return Err("--format parquet requires --output PATH".to_owned());
        }
        (CliOutputFormat::Text(_), Some(_)) => {
            return Err("--output is only supported with --format parquet".to_owned());
        }
        _ => {}
    }

    Ok(Some(Config {
        execute,
        format,
        output,
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
        assert_eq!(config.format, CliOutputFormat::Text(OutputFormat::Json));
        assert_eq!(config.execute.as_deref(), Some("SELECT * FROM t"));
    }

    #[test]
    fn rejects_unknown_formats() {
        let error = parse_arguments(["--format", "xml"].into_iter().map(str::to_owned))
            .expect_err("unknown format");
        assert!(error.contains("table, csv, json, or parquet"));
    }

    #[test]
    fn validates_parquet_output_options() {
        let missing_output =
            parse_arguments(["--format", "parquet"].into_iter().map(str::to_owned))
                .expect_err("missing Parquet output path");
        assert!(missing_output.contains("requires --output"));

        let text_output = parse_arguments(
            ["--format", "csv", "--output", "result.parquet"]
                .into_iter()
                .map(str::to_owned),
        )
        .expect_err("output path with text format");
        assert!(text_output.contains("only supported with --format parquet"));
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
