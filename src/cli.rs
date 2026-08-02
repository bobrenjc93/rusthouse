use std::env;
use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use rusthouse::Catalog;
use rusthouse::csv::write_csv;
use rusthouse::sql::{parse_create_table, parse_insert, parse_select};

const MAX_SCRIPT_BYTES: usize = 8 * 1024 * 1024;
const HELP: &str = "\
RustHouse batch SQL runner

Usage: rusthouse [FILE]

Read CREATE TABLE, INSERT INTO ... VALUES, and SELECT * statements from FILE.
With no FILE, or when FILE is -, read SQL from standard input. SELECT results
are written as CSV. Statements are executed in order in one in-memory catalog.

Options:
  -h, --help     Show this help
  -V, --version  Show the version
";

pub(crate) fn run() -> Result<(), CliError> {
    match parse_action(env::args_os().skip(1))? {
        Action::Help => write_stdout(HELP.as_bytes()),
        Action::Version => {
            write_stdout(format!("rusthouse {}\n", env!("CARGO_PKG_VERSION")).as_bytes())
        }
        Action::Execute(path) => {
            let script = read_script(path.as_deref())?;
            let stdout = io::stdout();
            execute_script(&script, &mut stdout.lock())
        }
    }
}

enum Action {
    Help,
    Version,
    Execute(Option<PathBuf>),
}

fn parse_action(args: impl Iterator<Item = OsString>) -> Result<Action, CliError> {
    let args = args.collect::<Vec<_>>();
    match args.as_slice() {
        [] => Ok(Action::Execute(None)),
        [argument] if argument == "-h" || argument == "--help" => Ok(Action::Help),
        [argument] if argument == "-V" || argument == "--version" => Ok(Action::Version),
        [argument] if argument == "-" => Ok(Action::Execute(None)),
        [argument] if argument.to_string_lossy().starts_with('-') => Err(CliError::Usage(format!(
            "unknown option {:?}; run with --help for usage",
            argument
        ))),
        [path] => Ok(Action::Execute(Some(PathBuf::from(path)))),
        _ => Err(CliError::Usage(
            "expected at most one SQL file; run with --help for usage".into(),
        )),
    }
}

fn read_script(path: Option<&Path>) -> Result<String, CliError> {
    match path {
        Some(path) => {
            let file = File::open(path).map_err(|source| CliError::Io {
                action: format!("open {}", path.display()),
                source,
            })?;
            read_bounded(file, &format!("read {}", path.display()))
        }
        None => read_bounded(io::stdin().lock(), "read standard input"),
    }
}

fn read_bounded(reader: impl Read, action: &str) -> Result<String, CliError> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_SCRIPT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| CliError::Io {
            action: action.to_owned(),
            source,
        })?;
    if bytes.len() > MAX_SCRIPT_BYTES {
        return Err(CliError::ScriptTooLarge {
            limit: MAX_SCRIPT_BYTES,
        });
    }
    String::from_utf8(bytes).map_err(CliError::InvalidUtf8)
}

fn write_stdout(bytes: &[u8]) -> Result<(), CliError> {
    io::stdout()
        .lock()
        .write_all(bytes)
        .map_err(|source| CliError::Io {
            action: "write standard output".into(),
            source,
        })
}

fn execute_script(script: &str, output: &mut dyn Write) -> Result<(), CliError> {
    let mut catalog = Catalog::default();
    for (offset, statement) in split_statements(script).into_iter().enumerate() {
        let number = offset + 1;
        let keyword = statement
            .trim_start()
            .bytes()
            .take_while(u8::is_ascii_alphabetic)
            .map(char::from)
            .collect::<String>();

        if keyword.eq_ignore_ascii_case("CREATE") {
            let parsed = parse_create_table(statement)
                .map_err(|error| CliError::statement(number, error))?;
            catalog
                .create_table(parsed)
                .map_err(|error| CliError::statement(number, error))?;
        } else if keyword.eq_ignore_ascii_case("INSERT") {
            let parsed =
                parse_insert(statement).map_err(|error| CliError::statement(number, error))?;
            catalog
                .insert(parsed)
                .map_err(|error| CliError::statement(number, error))?;
        } else if keyword.eq_ignore_ascii_case("SELECT") {
            let parsed =
                parse_select(statement).map_err(|error| CliError::statement(number, error))?;
            let table = catalog
                .select(parsed)
                .map_err(|error| CliError::statement(number, error))?;
            write_csv(table, output).map_err(|error| CliError::statement(number, error))?;
        } else {
            return Err(CliError::UnsupportedStatement { number, keyword });
        }
    }
    Ok(())
}

fn split_statements(script: &str) -> Vec<&str> {
    let bytes = script.as_bytes();
    let mut statements = Vec::new();
    let mut start = 0;
    let mut position = 0;
    let mut in_string = false;

    while position < bytes.len() {
        match bytes[position] {
            b'\'' if in_string && bytes.get(position + 1) == Some(&b'\'') => {
                position += 2;
            }
            b'\'' => {
                in_string = !in_string;
                position += 1;
            }
            b';' if !in_string => {
                push_nonempty(&mut statements, &script[start..=position]);
                position += 1;
                start = position;
            }
            _ => position += 1,
        }
    }
    push_nonempty(&mut statements, &script[start..]);
    statements
}

fn push_nonempty<'a>(statements: &mut Vec<&'a str>, statement: &'a str) {
    let statement = statement.trim();
    if !statement.is_empty() {
        statements.push(statement);
    }
}

#[derive(Debug)]
pub(crate) enum CliError {
    Usage(String),
    Io {
        action: String,
        source: io::Error,
    },
    InvalidUtf8(std::string::FromUtf8Error),
    ScriptTooLarge {
        limit: usize,
    },
    Statement {
        number: usize,
        source: Box<dyn Error + Send + Sync>,
    },
    UnsupportedStatement {
        number: usize,
        keyword: String,
    },
}

impl CliError {
    fn statement(number: usize, source: impl Error + Send + Sync + 'static) -> Self {
        Self::Statement {
            number,
            source: Box::new(source),
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message) => formatter.write_str(message),
            Self::Io { action, source } => write!(formatter, "failed to {action}: {source}"),
            Self::InvalidUtf8(_) => formatter.write_str("SQL input is not valid UTF-8"),
            Self::ScriptTooLarge { limit } => {
                write!(formatter, "SQL input exceeds the {limit}-byte script limit")
            }
            Self::Statement { number, source } => {
                write!(formatter, "statement {number} failed: {source}")
            }
            Self::UnsupportedStatement { number, keyword } if keyword.is_empty() => {
                write!(formatter, "statement {number} has no supported SQL keyword")
            }
            Self::UnsupportedStatement { number, keyword } => write!(
                formatter,
                "statement {number} uses unsupported keyword {keyword:?}"
            ),
        }
    }
}

impl Error for CliError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::InvalidUtf8(source) => Some(source),
            Self::Statement { source, .. } => Some(source.as_ref()),
            Self::Usage(_) | Self::ScriptTooLarge { .. } | Self::UnsupportedStatement { .. } => {
                None
            }
        }
    }
}
