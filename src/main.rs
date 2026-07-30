use std::env;
use std::io::{self, Read};
use std::process::ExitCode;

use rusthouse::format::{OutputFormat, render};
use rusthouse::{Database, ParseLimits, QueryResult, StatementResult};

const HELP: &str = "\
RustHouse - an in-memory columnar SQL engine

USAGE:
    rusthouse [OPTIONS]

OPTIONS:
    -e, --execute <SQL>       Execute SQL supplied as an argument
    -f, --format <FORMAT>     Output format: table (default), csv, or json
    --max-sql-bytes <N>       Maximum SQL input size
    --max-tokens <N>          Maximum lexical tokens per script
    --max-statements <N>      Maximum statements per script
    --max-identifier-bytes <N>  Maximum identifier length
    --max-literal-bytes <N>   Maximum literal length
    --max-schema-columns <N>  Maximum columns per CREATE TABLE
    --max-select-items <N>    Maximum items per SELECT list
    --max-values-cells <N>    Maximum cells per VALUES clause
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
        read_sql(io::stdin(), config.parse_limits.max_sql_bytes)?
    };

    let mut database = Database::with_parse_limits(config.parse_limits);
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
    parse_limits: ParseLimits,
}

fn parse_arguments(arguments: impl Iterator<Item = String>) -> Result<Option<Config>, String> {
    let mut execute = None;
    let mut format = OutputFormat::Table;
    let mut parse_limits = ParseLimits::default();
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
            _ if is_parse_limit_option(&argument) => {
                let value = arguments
                    .next()
                    .ok_or_else(|| format!("{argument} requires a non-negative integer"))?;
                set_parse_limit(&mut parse_limits, &argument, &value)?;
            }
            _ => {
                if let Some((option, value)) = argument.split_once('=')
                    && set_parse_limit(&mut parse_limits, option, value)?
                {
                    continue;
                }
                return Err(format!("unknown argument '{argument}'; try --help"));
            }
        }
    }

    Ok(Some(Config {
        execute,
        format,
        parse_limits,
    }))
}

fn is_parse_limit_option(option: &str) -> bool {
    matches!(
        option,
        "--max-sql-bytes"
            | "--max-tokens"
            | "--max-statements"
            | "--max-identifier-bytes"
            | "--max-literal-bytes"
            | "--max-schema-columns"
            | "--max-select-items"
            | "--max-values-cells"
    )
}

fn set_parse_limit(limits: &mut ParseLimits, option: &str, value: &str) -> Result<bool, String> {
    let target = match option {
        "--max-sql-bytes" => &mut limits.max_sql_bytes,
        "--max-tokens" => &mut limits.max_tokens,
        "--max-statements" => &mut limits.max_statements,
        "--max-identifier-bytes" => &mut limits.max_identifier_bytes,
        "--max-literal-bytes" => &mut limits.max_literal_bytes,
        "--max-schema-columns" => &mut limits.max_schema_columns,
        "--max-select-items" => &mut limits.max_select_items,
        "--max-values-cells" => &mut limits.max_values_cells,
        _ => return Ok(false),
    };
    *target = value.parse::<usize>().map_err(|_| {
        format!("invalid value '{value}' for {option}; expected a non-negative integer")
    })?;
    Ok(true)
}

fn read_sql(reader: impl Read, max_sql_bytes: usize) -> Result<String, String> {
    let read_limit = max_sql_bytes.saturating_add(1);
    let mut bytes = Vec::new();
    reader
        .take(u64::try_from(read_limit).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read SQL from stdin: {error}"))?;
    if bytes.len() > max_sql_bytes {
        return Err(format!("SQL input exceeds limit of {max_sql_bytes} bytes"));
    }
    String::from_utf8(bytes).map_err(|error| format!("could not read SQL from stdin: {error}"))
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
    fn parses_all_parse_limit_options() {
        let config = parse_arguments(
            [
                "--max-sql-bytes=1",
                "--max-tokens",
                "2",
                "--max-statements=3",
                "--max-identifier-bytes=4",
                "--max-literal-bytes=5",
                "--max-schema-columns=6",
                "--max-select-items=7",
                "--max-values-cells=8",
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .expect("valid arguments")
        .expect("not help");

        assert_eq!(
            config.parse_limits,
            ParseLimits {
                max_sql_bytes: 1,
                max_tokens: 2,
                max_statements: 3,
                max_identifier_bytes: 4,
                max_literal_bytes: 5,
                max_schema_columns: 6,
                max_select_items: 7,
                max_values_cells: 8,
            }
        );
    }

    #[test]
    fn stdin_reader_accepts_exact_byte_limit_and_rejects_the_next_byte() {
        assert_eq!(
            read_sql("SELECT".as_bytes(), 6).expect("exact boundary succeeds"),
            "SELECT"
        );
        let error = read_sql("SELECT".as_bytes(), 5).expect_err("one excess byte fails");
        assert!(error.contains("limit of 5 bytes"));
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
