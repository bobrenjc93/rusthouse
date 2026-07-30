use std::env;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use rusthouse::format::{OutputFormat, render};
use rusthouse::{
    DEFAULT_MAX_IN_MEMORY_GROUPS, Database, DatabaseOptions, QueryResult, StatementResult,
};

const HELP: &str = "\
RustHouse - an in-memory columnar SQL engine

USAGE:
    rusthouse [OPTIONS]

OPTIONS:
    -e, --execute <SQL>       Execute SQL supplied as an argument
    -f, --format <FORMAT>     Output format: table (default), csv, or json
        --max-in-memory-groups <COUNT>
                              Spill GROUP BY above this many groups (default: 65536)
        --temporary-directory <PATH>
                              Parent directory for GROUP BY spill files
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

    let mut database = Database::with_options(DatabaseOptions {
        max_in_memory_groups: config.max_in_memory_groups,
        temporary_directory: config.temporary_directory,
    });
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
    max_in_memory_groups: usize,
    temporary_directory: Option<PathBuf>,
}

fn parse_arguments(arguments: impl Iterator<Item = String>) -> Result<Option<Config>, String> {
    let mut execute = None;
    let mut format = OutputFormat::Table;
    let mut max_in_memory_groups = DEFAULT_MAX_IN_MEMORY_GROUPS;
    let mut max_in_memory_groups_supplied = false;
    let mut temporary_directory = None;
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
            "--max-in-memory-groups" => {
                if max_in_memory_groups_supplied {
                    return Err("--max-in-memory-groups may only be supplied once".to_owned());
                }
                let value = arguments
                    .next()
                    .ok_or_else(|| format!("{argument} requires a count"))?;
                max_in_memory_groups = parse_max_in_memory_groups(&value)?;
                max_in_memory_groups_supplied = true;
            }
            "--temporary-directory" => {
                if temporary_directory.is_some() {
                    return Err("--temporary-directory may only be supplied once".to_owned());
                }
                temporary_directory = Some(PathBuf::from(
                    arguments
                        .next()
                        .ok_or_else(|| format!("{argument} requires a path"))?,
                ));
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
            _ if argument.starts_with("--max-in-memory-groups=") => {
                if max_in_memory_groups_supplied {
                    return Err("--max-in-memory-groups may only be supplied once".to_owned());
                }
                max_in_memory_groups =
                    parse_max_in_memory_groups(&argument["--max-in-memory-groups=".len()..])?;
                max_in_memory_groups_supplied = true;
            }
            _ if argument.starts_with("--temporary-directory=") => {
                if temporary_directory.is_some() {
                    return Err("--temporary-directory may only be supplied once".to_owned());
                }
                temporary_directory =
                    Some(PathBuf::from(&argument["--temporary-directory=".len()..]));
            }
            _ => return Err(format!("unknown argument '{argument}'; try --help")),
        }
    }

    Ok(Some(Config {
        execute,
        format,
        max_in_memory_groups,
        temporary_directory,
    }))
}

fn parse_max_in_memory_groups(value: &str) -> Result<usize, String> {
    match value.parse::<usize>() {
        Ok(0) => Err("--max-in-memory-groups must be at least 1".to_owned()),
        Ok(value) => Ok(value),
        Err(_) => Err(format!(
            "invalid --max-in-memory-groups value '{value}'; expected a positive integer"
        )),
    }
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
        assert_eq!(config.max_in_memory_groups, DEFAULT_MAX_IN_MEMORY_GROUPS);
    }

    #[test]
    fn rejects_unknown_formats() {
        let error = parse_arguments(["--format", "xml"].into_iter().map(str::to_owned))
            .expect_err("unknown format");
        assert!(error.contains("table, csv, or json"));
    }

    #[test]
    fn parses_grouping_spill_options() {
        let config = parse_arguments(
            [
                "--max-in-memory-groups=7",
                "--temporary-directory",
                "/tmp/rusthouse-test",
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .expect("valid arguments")
        .expect("not help");

        assert_eq!(config.max_in_memory_groups, 7);
        assert_eq!(
            config.temporary_directory,
            Some(PathBuf::from("/tmp/rusthouse-test"))
        );
        assert!(parse_max_in_memory_groups("0").is_err());
        assert!(parse_max_in_memory_groups("many").is_err());
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
