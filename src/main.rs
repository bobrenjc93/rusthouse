use std::env;
use std::fmt;
use std::io::{self, BufWriter, Read, Write};
use std::process::ExitCode;

use rusthouse::format::{CsvSink, JsonSink, OutputFormat, render};
use rusthouse::{
    Database, ExecutionError, QueryResult, ResultColumn, RowSink, StatementResult, ValueRef,
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
        Err(CliError::Io(error)) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), CliError> {
    let Some(config) = parse_arguments(env::args().skip(1)).map_err(CliError::Message)? else {
        let mut stdout = io::stdout().lock();
        stdout.write_all(HELP.as_bytes()).map_err(CliError::Io)?;
        stdout.flush().map_err(CliError::Io)?;
        return Ok(());
    };

    let sql = if let Some(sql) = config.execute {
        sql
    } else {
        let mut sql = String::new();
        io::stdin().read_to_string(&mut sql).map_err(|error| {
            CliError::Message(format!("could not read SQL from stdin: {error}"))
        })?;
        sql
    };

    let mut database = Database::new();
    match config.format {
        OutputFormat::Table => run_table(&mut database, &sql),
        OutputFormat::Csv => {
            let stdout = io::stdout();
            let output = BufWriter::new(stdout.lock());
            let mut sink = CommandSink::new(CsvSink::new(output));
            database
                .execute_with_sink(&sql, &mut sink)
                .map_err(map_execution_error)?;
            write_commands(&sink.commands)?;
            sink.output.get_mut().flush().map_err(CliError::Io)
        }
        OutputFormat::Json => {
            let stdout = io::stdout();
            let output = BufWriter::new(stdout.lock());
            let mut sink = CommandSink::new(JsonSink::new(output));
            let execution = database.execute_with_sink(&sql, &mut sink);
            let finalization = sink.output.finish();
            if let Err(error) = execution {
                let _ = sink.output.get_mut().flush();
                return Err(map_execution_error(error));
            }
            finalization.map_err(CliError::Io)?;
            write_commands(&sink.commands)?;
            sink.output.get_mut().flush().map_err(CliError::Io)
        }
    }
}

fn run_table(database: &mut Database, sql: &str) -> Result<(), CliError> {
    let results = database
        .execute(sql)
        .map_err(|error| CliError::Message(error.to_string()))?;
    let mut queries = Vec::new();
    for result in results {
        match result {
            StatementResult::Command { tag, affected_rows } => {
                write_commands(&[(tag, affected_rows)])?;
            }
            StatementResult::Query(result) => queries.push(result),
        }
    }
    let rendered = render_query_results(&queries, OutputFormat::Table);
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(rendered.as_bytes())
        .map_err(CliError::Io)?;
    stdout.flush().map_err(CliError::Io)
}

fn write_commands(commands: &[(&'static str, usize)]) -> Result<(), CliError> {
    let mut stderr = io::stderr().lock();
    for (tag, affected_rows) in commands {
        if *tag == "INSERT" {
            writeln!(stderr, "{tag} {affected_rows}").map_err(CliError::Io)?;
        } else {
            writeln!(stderr, "{tag}").map_err(CliError::Io)?;
        }
    }
    stderr.flush().map_err(CliError::Io)
}

fn map_execution_error(error: ExecutionError<io::Error>) -> CliError {
    match error {
        ExecutionError::Database(error) => CliError::Message(error.to_string()),
        ExecutionError::Sink(error) => CliError::Io(error),
    }
}

#[derive(Debug)]
enum CliError {
    Message(String),
    Io(io::Error),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(message) => formatter.write_str(message),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

#[derive(Debug)]
struct CommandSink<S> {
    output: S,
    commands: Vec<(&'static str, usize)>,
}

impl<S> CommandSink<S> {
    fn new(output: S) -> Self {
        Self {
            output,
            commands: Vec::new(),
        }
    }
}

impl<S: RowSink> RowSink for CommandSink<S> {
    type Error = S::Error;

    fn command(&mut self, tag: &'static str, affected_rows: usize) -> Result<(), Self::Error> {
        self.commands.push((tag, affected_rows));
        Ok(())
    }

    fn begin_query(&mut self, columns: &[ResultColumn]) -> Result<(), Self::Error> {
        self.output.begin_query(columns)
    }

    fn row<'a, I>(&mut self, values: I) -> Result<(), Self::Error>
    where
        I: ExactSizeIterator<Item = ValueRef<'a>>,
    {
        self.output.row(values)
    }

    fn end_query(&mut self) -> Result<(), Self::Error> {
        self.output.end_query()
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
