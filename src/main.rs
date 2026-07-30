use std::env;
use std::io::{self, Read};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use rusthouse::format::{OutputFormat, render};
use rusthouse::{Database, ExecutionLimits, QueryResult, StatementResult};

const HELP: &str = "\
RustHouse - an in-memory columnar SQL engine

USAGE:
    rusthouse [OPTIONS]

OPTIONS:
    -e, --execute <SQL>       Execute SQL supplied as an argument
    -f, --format <FORMAT>     Output format: table (default), csv, or json
        --max-scan-rows <N>   Abort after scanning N source rows
        --max-output-rows <N> Abort before emitting more than N result rows
        --timeout-ms <MS>     Abort after this execution time in milliseconds
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
    let deadline = config
        .timeout_ms
        .map(|milliseconds| {
            Instant::now()
                .checked_add(Duration::from_millis(milliseconds))
                .ok_or_else(|| "--timeout-ms is too large for this platform".to_owned())
        })
        .transpose()?;
    let limits = ExecutionLimits {
        max_scan_rows: config.max_scan_rows,
        max_output_rows: config.max_output_rows,
        deadline,
    };
    let results = if limits == ExecutionLimits::unlimited() {
        database.execute(&sql)
    } else {
        database.execute_with_options(&sql, limits)
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
    format: OutputFormat,
    max_scan_rows: Option<usize>,
    max_output_rows: Option<usize>,
    timeout_ms: Option<u64>,
}

fn parse_arguments(arguments: impl Iterator<Item = String>) -> Result<Option<Config>, String> {
    let mut execute = None;
    let mut format = OutputFormat::Table;
    let mut max_scan_rows = None;
    let mut max_output_rows = None;
    let mut timeout_ms = None;
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
            "--max-scan-rows" => {
                if max_scan_rows.is_some() {
                    return Err("--max-scan-rows may only be supplied once".to_owned());
                }
                let value = arguments
                    .next()
                    .ok_or_else(|| "--max-scan-rows requires a row count".to_owned())?;
                max_scan_rows = Some(parse_number("--max-scan-rows", &value)?);
            }
            "--max-output-rows" => {
                if max_output_rows.is_some() {
                    return Err("--max-output-rows may only be supplied once".to_owned());
                }
                let value = arguments
                    .next()
                    .ok_or_else(|| "--max-output-rows requires a row count".to_owned())?;
                max_output_rows = Some(parse_number("--max-output-rows", &value)?);
            }
            "--timeout-ms" => {
                if timeout_ms.is_some() {
                    return Err("--timeout-ms may only be supplied once".to_owned());
                }
                let value = arguments
                    .next()
                    .ok_or_else(|| "--timeout-ms requires a millisecond count".to_owned())?;
                timeout_ms = Some(parse_number("--timeout-ms", &value)?);
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
            _ if argument.starts_with("--max-scan-rows=") => {
                if max_scan_rows.is_some() {
                    return Err("--max-scan-rows may only be supplied once".to_owned());
                }
                max_scan_rows = Some(parse_number(
                    "--max-scan-rows",
                    &argument["--max-scan-rows=".len()..],
                )?);
            }
            _ if argument.starts_with("--max-output-rows=") => {
                if max_output_rows.is_some() {
                    return Err("--max-output-rows may only be supplied once".to_owned());
                }
                max_output_rows = Some(parse_number(
                    "--max-output-rows",
                    &argument["--max-output-rows=".len()..],
                )?);
            }
            _ if argument.starts_with("--timeout-ms=") => {
                if timeout_ms.is_some() {
                    return Err("--timeout-ms may only be supplied once".to_owned());
                }
                timeout_ms = Some(parse_number(
                    "--timeout-ms",
                    &argument["--timeout-ms=".len()..],
                )?);
            }
            _ => return Err(format!("unknown argument '{argument}'; try --help")),
        }
    }

    Ok(Some(Config {
        execute,
        format,
        max_scan_rows,
        max_output_rows,
        timeout_ms,
    }))
}

fn parse_number<T>(option: &str, value: &str) -> Result<T, String>
where
    T: std::str::FromStr,
{
    value.parse().map_err(|_| {
        format!("invalid value '{value}' for {option}; expected a non-negative integer")
    })
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
        assert_eq!(config.max_scan_rows, None);
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
