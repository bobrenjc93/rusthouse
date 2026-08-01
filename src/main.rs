use std::env;
use std::error::Error;
use std::io::{self, BufRead};
use std::path::PathBuf;
use std::process::ExitCode;

use rusthouse::{Database, ResultSet, StatementResult, Value};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rusthouse: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let Some(options) = Options::parse(env::args().skip(1))? else {
        print_help();
        return Ok(());
    };
    let database = match options.database {
        Some(path) => Database::open(path)?,
        None => Database::new(),
    };
    let mut session = database.session();
    if options.statements.is_empty() {
        for line in io::stdin().lock().lines() {
            let line = line?;
            if !line.trim().is_empty() {
                print_result(session.execute(&line)?);
            }
        }
    } else {
        for statement in options.statements {
            print_result(session.execute(&statement)?);
        }
    }
    Ok(())
}

struct Options {
    database: Option<PathBuf>,
    statements: Vec<String>,
}

impl Options {
    fn parse(mut arguments: impl Iterator<Item = String>) -> io::Result<Option<Self>> {
        let mut database = None;
        let mut statements = Vec::new();
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "-h" | "--help" => return Ok(None),
                "-d" | "--database" => {
                    let path = arguments.next().ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "--database requires a file path",
                        )
                    })?;
                    database = Some(PathBuf::from(path));
                }
                "-e" | "--execute" => {
                    statements.push(arguments.next().ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "--execute requires a SQL statement",
                        )
                    })?);
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
        }))
    }
}

fn print_result(result: StatementResult) {
    match result {
        StatementResult::TransactionStarted { generation } => {
            println!("BEGIN (generation {generation})");
        }
        StatementResult::TransactionCommitted { generation } => {
            println!("COMMIT (generation {generation})");
        }
        StatementResult::TransactionRolledBack => println!("ROLLBACK"),
        StatementResult::TableCreated => println!("CREATE TABLE"),
        StatementResult::TableDropped => println!("DROP TABLE"),
        StatementResult::RowsInserted { rows } => println!("INSERT {rows}"),
        StatementResult::Query(result) => print_rows(&result),
    }
}

fn print_rows(result: &ResultSet) {
    println!(
        "{}",
        result
            .columns
            .iter()
            .map(|column| escape_field(&column.name))
            .collect::<Vec<_>>()
            .join("\t")
    );
    for row in &result.rows {
        println!(
            "{}",
            row.iter().map(format_value).collect::<Vec<_>>().join("\t")
        );
    }
    println!("{} row(s)", result.row_count());
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

fn print_help() {
    println!("{}", rusthouse::product_name());
    println!("Usage: rusthouse [--database FILE] [-e SQL]...");
    println!("Without -e, one SQL statement is read from each input line.");
}
