use std::env;
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::process::ExitCode;

use rusthouse::format::{OutputFormat, render};
use rusthouse::sql::StatementFramer;
use rusthouse::{Database, QueryResult, StatementResult};

const HELP: &str = "\
RustHouse - an in-memory columnar SQL engine

USAGE:
    rusthouse [OPTIONS]

OPTIONS:
    -e, --execute <SQL>       Execute SQL supplied as an argument
        --interactive         Start a stateful, multiline SQL shell
    -f, --format <FORMAT>     Output format: table (default), csv, or json
    -h, --help                Print this help

With no --execute option, SQL is read to EOF from standard input.
Interactive statements end with ';'; exit with EOF or .quit.
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

    if config.interactive {
        let stdin = io::stdin();
        let show_prompts = stdin.is_terminal();
        return run_interactive(
            stdin.lock(),
            io::stdout().lock(),
            io::stderr().lock(),
            config.format,
            show_prompts,
        );
    }

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
    write_command_results(&results, &mut io::stderr().lock())?;
    let queries = query_results(results);
    print!("{}", render_query_results(&queries, config.format));
    Ok(())
}

fn run_interactive(
    mut input: impl BufRead,
    mut output: impl Write,
    mut errors: impl Write,
    format: OutputFormat,
    show_prompts: bool,
) -> Result<(), String> {
    let mut database = Database::new();
    let mut framer = StatementFramer::new();
    let mut line = String::new();

    loop {
        if show_prompts {
            let prompt = if framer.is_idle() {
                "rusthouse> "
            } else {
                "       ...> "
            };
            errors
                .write_all(prompt.as_bytes())
                .and_then(|()| errors.flush())
                .map_err(|error| format!("could not write prompt: {error}"))?;
        }

        line.clear();
        let bytes_read = input
            .read_line(&mut line)
            .map_err(|error| format!("could not read SQL from stdin: {error}"))?;
        if bytes_read == 0 || (framer.is_idle() && line.trim() == ".quit") {
            if show_prompts && bytes_read == 0 {
                errors
                    .write_all(b"\n")
                    .map_err(|error| format!("could not write prompt: {error}"))?;
            }
            return Ok(());
        }

        for statement in framer.push_str(&line) {
            match database.execute(&statement) {
                Ok(results) => {
                    write_command_results(&results, &mut errors)?;
                    let queries = query_results(results);
                    if !queries.is_empty() {
                        let rendered = render_query_results(&queries, format);
                        output
                            .write_all(rendered.as_bytes())
                            .and_then(|()| output.flush())
                            .map_err(|error| format!("could not write query output: {error}"))?;
                    }
                    errors
                        .flush()
                        .map_err(|error| format!("could not write command output: {error}"))?;
                }
                Err(error) => {
                    writeln!(errors, "error: {error}")
                        .and_then(|()| errors.flush())
                        .map_err(|error| format!("could not write SQL error: {error}"))?;
                }
            }
        }
    }
}

fn write_command_results(
    results: &[StatementResult],
    errors: &mut impl Write,
) -> Result<(), String> {
    for result in results {
        if let StatementResult::Command { tag, affected_rows } = result {
            if *tag == "INSERT" {
                writeln!(errors, "{tag} {affected_rows}")
            } else {
                writeln!(errors, "{tag}")
            }
            .map_err(|error| format!("could not write command output: {error}"))?;
        }
    }
    Ok(())
}

fn query_results(results: Vec<StatementResult>) -> Vec<QueryResult> {
    results
        .into_iter()
        .filter_map(|result| match result {
            StatementResult::Command { .. } => None,
            StatementResult::Query(result) => Some(result),
        })
        .collect()
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
    interactive: bool,
    format: OutputFormat,
}

fn parse_arguments(arguments: impl Iterator<Item = String>) -> Result<Option<Config>, String> {
    let mut execute = None;
    let mut interactive = false;
    let mut format = OutputFormat::Table;
    let mut arguments = arguments.peekable();

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "-h" | "--help" => return Ok(None),
            "--interactive" => interactive = true,
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
        return Err("--interactive cannot be used with --execute".to_owned());
    }

    Ok(Some(Config {
        execute,
        interactive,
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
