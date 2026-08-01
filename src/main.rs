use std::{
    env,
    error::Error,
    io::{self, BufRead, Write},
    net::SocketAddr,
    path::PathBuf,
    process::ExitCode,
    sync::Arc,
    time::Duration,
};

use rusthouse::{
    Database, ResultSet, StatementResult, Value,
    http::{ServerConfig, spawn_http_server},
};

const HELP: &str = "\
RustHouse analytical database

Usage:
  rusthouse [--database FILE] [-e SQL]...
  rusthouse serve [OPTIONS]

Query options:
  -d, --database FILE            Persist data in FILE
  -e, --execute SQL              Execute SQL; repeat to share one session
  -f, --format FORMAT            Output format: table (default) or csv
  -h, --help                     Print help

Without --execute, one SQL statement is read from each input line.

Server options:
  -d, --database FILE            Persist data in FILE
  --bind ADDRESS                 Listen address [default: 127.0.0.1:8080]
  --max-request-bytes BYTES      Maximum SQL request size [default: 1048576]
  --max-response-bytes BYTES     Maximum encoded result size [default: 16777216]
  --max-concurrent-queries N     Query execution slots [default: 16]
  --max-concurrent-requests N    HTTP query request slots [default: 64]
  --max-connections N            Accepted client connections [default: 128]
  --header-read-timeout-ms MS    HTTP header deadline [default: 10000]
  --connection-idle-timeout-ms MS  Socket idle deadline [default: 60000]
  --request-body-timeout-ms MS   Request body deadline [default: 10000]
  --query-timeout-ms MS          Per-query deadline [default: 30000]
  --shutdown-timeout-ms MS       Graceful shutdown window [default: 10000]
  -h, --help                     Print help
";

#[tokio::main]
async fn main() -> ExitCode {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    if arguments
        .first()
        .is_some_and(|argument| argument == "serve")
    {
        match run_server(arguments.into_iter().skip(1)).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("rusthouse: {error}");
                ExitCode::from(2)
            }
        }
    } else {
        match run_cli(arguments.into_iter()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("rusthouse: {error}");
                ExitCode::FAILURE
            }
        }
    }
}

fn run_cli(arguments: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let Some(options) = CliOptions::parse(arguments)? else {
        print!("{HELP}");
        return Ok(());
    };
    let database = open_database(options.database)?;
    let mut session = database.session();
    let stdout = io::stdout();
    let mut output = io::BufWriter::new(stdout.lock());
    if options.statements.is_empty() {
        for line in io::stdin().lock().lines() {
            let line = line?;
            if !line.trim().is_empty() {
                write_result(&mut output, session.execute(&line)?, options.format)?;
            }
        }
    } else {
        for statement in options.statements {
            write_result(&mut output, session.execute(&statement)?, options.format)?;
        }
    }
    output.flush()?;
    Ok(())
}

fn open_database(path: Option<PathBuf>) -> rusthouse::Result<Database> {
    match path {
        Some(path) => Database::open(path),
        None => Ok(Database::new()),
    }
}

struct CliOptions {
    database: Option<PathBuf>,
    statements: Vec<String>,
    format: OutputFormat,
}

impl CliOptions {
    fn parse(mut arguments: impl Iterator<Item = String>) -> io::Result<Option<Self>> {
        let mut database = None;
        let mut statements = Vec::new();
        let mut format = OutputFormat::Table;
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "-h" | "--help" => return Ok(None),
                "-d" | "--database" => {
                    database = Some(PathBuf::from(required_argument(
                        &mut arguments,
                        "--database requires a file path",
                    )?));
                }
                "-e" | "--execute" => {
                    statements.push(required_argument(
                        &mut arguments,
                        "--execute requires a SQL statement",
                    )?);
                }
                "-f" | "--format" => {
                    let value =
                        required_argument(&mut arguments, "--format requires an output format")?;
                    format = OutputFormat::parse(&value)?;
                }
                _ if argument.starts_with("--format=") => {
                    format = OutputFormat::parse(&argument["--format=".len()..])?;
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unknown argument {argument:?}"),
                    ));
                }
            }
        }
        Ok(Some(Self {
            database,
            statements,
            format,
        }))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputFormat {
    Table,
    CsvWithNames,
}

impl OutputFormat {
    fn parse(value: &str) -> io::Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "table" => Ok(Self::Table),
            "csv" | "csvwithnames" => Ok(Self::CsvWithNames),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown output format {value:?}; expected table, csv, or CSVWithNames"),
            )),
        }
    }
}

fn required_argument(
    arguments: &mut impl Iterator<Item = String>,
    message: &'static str,
) -> io::Result<String> {
    arguments
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, message))
}

struct ServerOptions {
    address: SocketAddr,
    database: Option<PathBuf>,
    config: ServerConfig,
}

impl ServerOptions {
    fn parse(mut arguments: impl Iterator<Item = String>) -> Result<Option<Self>, String> {
        let mut address = "127.0.0.1:8080"
            .parse()
            .expect("the default address is valid");
        let mut database = None;
        let mut config = ServerConfig::default();

        while let Some(option) = arguments.next() {
            if matches!(option.as_str(), "-h" | "--help") {
                return Ok(None);
            }
            match option.as_str() {
                "-d" | "--database" => {
                    database = Some(PathBuf::from(server_value(&mut arguments, &option)?));
                }
                "--bind" => {
                    address = server_value(&mut arguments, &option)?
                        .parse()
                        .map_err(|error| format!("invalid --bind address: {error}"))?;
                }
                "--max-request-bytes" => {
                    config.max_request_bytes = parse_server_number(&mut arguments, &option)?;
                }
                "--max-response-bytes" => {
                    config.max_response_bytes = parse_server_number(&mut arguments, &option)?;
                }
                "--max-concurrent-queries" => {
                    config.max_concurrent_queries = parse_server_number(&mut arguments, &option)?;
                }
                "--max-concurrent-requests" => {
                    config.max_concurrent_requests = parse_server_number(&mut arguments, &option)?;
                }
                "--max-connections" => {
                    config.max_connections = parse_server_number(&mut arguments, &option)?;
                }
                "--header-read-timeout-ms" => {
                    config.header_read_timeout =
                        Duration::from_millis(parse_server_number(&mut arguments, &option)?);
                }
                "--connection-idle-timeout-ms" => {
                    config.connection_idle_timeout =
                        Duration::from_millis(parse_server_number(&mut arguments, &option)?);
                }
                "--request-body-timeout-ms" => {
                    config.request_body_timeout =
                        Duration::from_millis(parse_server_number(&mut arguments, &option)?);
                }
                "--query-timeout-ms" => {
                    config.query_timeout =
                        Duration::from_millis(parse_server_number(&mut arguments, &option)?);
                }
                "--shutdown-timeout-ms" => {
                    config.shutdown_timeout =
                        Duration::from_millis(parse_server_number(&mut arguments, &option)?);
                }
                _ => return Err(format!("unknown option `{option}`")),
            }
        }

        Ok(Some(Self {
            address,
            database,
            config,
        }))
    }
}

fn server_value(
    arguments: &mut impl Iterator<Item = String>,
    option: &str,
) -> Result<String, String> {
    arguments
        .next()
        .ok_or_else(|| format!("missing value for `{option}`"))
}

fn parse_server_number<T>(
    arguments: &mut impl Iterator<Item = String>,
    option: &str,
) -> Result<T, String>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let value = server_value(arguments, option)?;
    value
        .parse()
        .map_err(|error| format!("invalid value for `{option}`: {error}"))
}

async fn run_server(arguments: impl Iterator<Item = String>) -> Result<(), String> {
    let Some(options) = ServerOptions::parse(arguments)? else {
        print!("{HELP}");
        return Ok(());
    };
    let database = open_database(options.database).map_err(|error| error.to_string())?;
    let mut shutdown_signals = ShutdownSignals::new()?;
    let mut server = spawn_http_server(options.address, Arc::new(database), options.config)
        .await
        .map_err(|error| error.to_string())?;
    eprintln!(
        "RustHouse HTTP server listening on http://{}",
        server.local_addr()
    );
    let signal = tokio::select! {
        biased;
        result = server.wait() => {
            result.map_err(|error| error.to_string())?;
            return Err("HTTP server stopped unexpectedly".into());
        }
        result = shutdown_signals.wait() => result,
    };
    signal?;
    server.shutdown().await.map_err(|error| error.to_string())
}

struct ShutdownSignals {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
}

impl ShutdownSignals {
    fn new() -> Result<Self, String> {
        #[cfg(unix)]
        {
            let interrupt =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                    .map_err(|error| format!("could not listen for SIGINT: {error}"))?;
            let terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .map_err(|error| format!("could not listen for SIGTERM: {error}"))?;
            Ok(Self {
                interrupt,
                terminate,
            })
        }

        #[cfg(not(unix))]
        Ok(Self {})
    }

    async fn wait(&mut self) -> Result<(), String> {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.interrupt.recv() => Ok(()),
                _ = self.terminate.recv() => Ok(()),
            }
        }

        #[cfg(not(unix))]
        tokio::signal::ctrl_c()
            .await
            .map_err(|error| format!("could not listen for Ctrl-C: {error}"))
    }
}

fn write_result(
    output: &mut impl Write,
    result: StatementResult,
    format: OutputFormat,
) -> io::Result<()> {
    if format == OutputFormat::CsvWithNames {
        return match result {
            StatementResult::Query(result) => write_csv_with_names(output, &result),
            _ => Ok(()),
        };
    }

    match result {
        StatementResult::TransactionStarted { generation } => {
            writeln!(output, "BEGIN (generation {generation})")?;
        }
        StatementResult::TransactionCommitted { generation } => {
            writeln!(output, "COMMIT (generation {generation})")?;
        }
        StatementResult::TransactionRolledBack => writeln!(output, "ROLLBACK")?,
        StatementResult::TableCreated => writeln!(output, "CREATE TABLE")?,
        StatementResult::TableDropped => writeln!(output, "DROP TABLE")?,
        StatementResult::RowsInserted { rows } => writeln!(output, "INSERT {rows}")?,
        StatementResult::Query(result) => write_table_rows(output, &result)?,
    }
    Ok(())
}

fn write_table_rows(output: &mut impl Write, result: &ResultSet) -> io::Result<()> {
    writeln!(
        output,
        "{}",
        result
            .columns
            .iter()
            .map(|column| escape_field(&column.name))
            .collect::<Vec<_>>()
            .join("\t")
    )?;
    for row in &result.rows {
        writeln!(
            output,
            "{}",
            row.iter().map(format_value).collect::<Vec<_>>().join("\t")
        )?;
    }
    writeln!(output, "{} row(s)", result.row_count())
}

fn write_csv_with_names(output: &mut impl Write, result: &ResultSet) -> io::Result<()> {
    write_csv_record(
        output,
        result
            .columns
            .iter()
            .map(|column| CsvField::String(&column.name)),
    )?;
    for row in &result.rows {
        write_csv_record(output, row.iter().map(csv_value))?;
    }
    Ok(())
}

enum CsvField<'a> {
    String(&'a str),
    Scalar(String),
    Null,
}

fn csv_value(value: &Value) -> CsvField<'_> {
    match value {
        Value::Null => CsvField::Null,
        Value::String(value) => CsvField::String(value),
        value => CsvField::Scalar(format_value(value)),
    }
}

fn write_csv_record<'a>(
    output: &mut impl Write,
    fields: impl Iterator<Item = CsvField<'a>>,
) -> io::Result<()> {
    for (index, field) in fields.enumerate() {
        if index > 0 {
            output.write_all(b",")?;
        }
        match field {
            CsvField::String(value) => write_quoted_csv_field(output, value)?,
            CsvField::Scalar(value) => output.write_all(value.as_bytes())?,
            CsvField::Null => output.write_all(b"\\N")?,
        }
    }
    output.write_all(b"\n")
}

fn write_quoted_csv_field(output: &mut impl Write, value: &str) -> io::Result<()> {
    output.write_all(b"\"")?;
    for part in value.split_inclusive('"') {
        output.write_all(part.as_bytes())?;
        if part.ends_with('"') {
            output.write_all(b"\"")?;
        }
    }
    output.write_all(b"\"")
}

fn format_value(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Int64(value) => value.to_string(),
        Value::Float64(value) => value.to_string(),
        Value::Bool(value) => value.to_string(),
        Value::String(value) => escape_field(value),
    }
}

fn escape_field(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}
