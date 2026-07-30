use std::env;
use std::io::{self, BufWriter, Read, Write};
use std::process::ExitCode;

use rusthouse::format::{
    OutputFormat, render, write_csv_header, write_csv_rows, write_json_query_end,
    write_json_query_start, write_json_rows,
};
use rusthouse::{
    Database, ExecuteError, QueryResult, ResultColumn, ResultSink, StatementResult, Value,
};

const HELP: &str = "\
RustHouse - an in-memory columnar SQL engine

USAGE:
    rusthouse [OPTIONS]

OPTIONS:
    -e, --execute <SQL>       Execute SQL supplied as an argument
    -f, --format <FORMAT>     Output format: table (default), csv, or json
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
    let stdout = io::stdout();
    let mut output = BufWriter::new(stdout.lock());
    if config.format == OutputFormat::Table {
        let results = database.execute(&sql).map_err(|error| error.to_string())?;
        let mut queries = Vec::new();
        for result in results {
            match result {
                StatementResult::Command { tag, affected_rows } => {
                    report_command(tag, affected_rows);
                }
                StatementResult::Query(result) => queries.push(result),
            }
        }
        output
            .write_all(render_query_results(&queries, config.format).as_bytes())
            .map_err(|error| format!("could not write query output: {error}"))?;
    } else {
        let deferred_error = {
            let mut sink = CliSink::new(&mut output, config.format);
            match database.execute_into(&sql, &mut sink) {
                Ok(()) => {
                    sink.finish()
                        .map_err(|error| format!("could not write query output: {error}"))?;
                    None
                }
                Err(error) => {
                    let mut message = error.to_string();
                    if matches!(&error, ExecuteError::Database(_))
                        && let Err(finalize_error) = sink.finish_after_database_error()
                    {
                        message.push_str(&format!(
                            "; could not finalize query output: {finalize_error}"
                        ));
                    }
                    Some(message)
                }
            }
        };
        if let Some(error) = deferred_error {
            output.flush().map_err(|flush_error| {
                format!("{error}; could not flush query output: {flush_error}")
            })?;
            return Err(error);
        }
    }
    output
        .flush()
        .map_err(|error| format!("could not flush query output: {error}"))?;
    Ok(())
}

fn report_command(tag: &'static str, affected_rows: usize) {
    if tag == "INSERT" {
        eprintln!("{tag} {affected_rows}");
    } else {
        eprintln!("{tag}");
    }
}

struct CliSink<'a, W> {
    output: &'a mut W,
    format: OutputFormat,
    query_count: usize,
    first_row: bool,
}

impl<'a, W: Write> CliSink<'a, W> {
    fn new(output: &'a mut W, format: OutputFormat) -> Self {
        debug_assert!(format != OutputFormat::Table);
        Self {
            output,
            format,
            query_count: 0,
            first_row: true,
        }
    }

    fn finish(&mut self) -> io::Result<()> {
        if self.format == OutputFormat::Json {
            if self.query_count == 0 {
                self.output.write_all(b"{\"results\":[]}")?;
            } else {
                self.output.write_all(b"]}")?;
            }
            self.output.write_all(b"\n")?;
        }
        Ok(())
    }

    fn finish_after_database_error(&mut self) -> io::Result<()> {
        if self.format == OutputFormat::Json && self.query_count > 0 {
            self.finish()?;
        }
        Ok(())
    }
}

impl<W: Write> ResultSink for CliSink<'_, W> {
    type Error = io::Error;

    fn command(&mut self, tag: &'static str, affected_rows: usize) -> io::Result<()> {
        report_command(tag, affected_rows);
        Ok(())
    }

    fn begin_query(&mut self, columns: &[ResultColumn]) -> io::Result<()> {
        if self.query_count > 0 {
            match self.format {
                OutputFormat::Csv => self.output.write_all(b"\n")?,
                OutputFormat::Json => self.output.write_all(b",")?,
                OutputFormat::Table => unreachable!("table results use the collecting adapter"),
            }
        } else if self.format == OutputFormat::Json {
            self.output.write_all(b"{\"results\":[")?;
        }
        self.query_count += 1;
        self.first_row = true;

        match self.format {
            OutputFormat::Csv => write_csv_header(self.output, columns),
            OutputFormat::Json => write_json_query_start(self.output, columns),
            OutputFormat::Table => unreachable!("table results use the collecting adapter"),
        }
    }

    fn rows(&mut self, rows: &[Vec<Value>]) -> io::Result<()> {
        match self.format {
            OutputFormat::Csv => write_csv_rows(self.output, rows),
            OutputFormat::Json => write_json_rows(self.output, rows, &mut self.first_row),
            OutputFormat::Table => unreachable!("table results use the collecting adapter"),
        }
    }

    fn end_query(&mut self) -> io::Result<()> {
        match self.format {
            OutputFormat::Csv => Ok(()),
            OutputFormat::Json => write_json_query_end(self.output),
            OutputFormat::Table => unreachable!("table results use the collecting adapter"),
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
