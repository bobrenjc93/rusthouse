use std::fmt;

/// Errors returned by parsing, catalog operations, execution, and front ends.
#[derive(Debug)]
pub enum Error {
    Lex {
        position: usize,
        message: String,
    },
    Parse {
        position: usize,
        message: String,
    },
    Catalog(String),
    Type(String),
    Execution(String),
    Limit {
        resource: &'static str,
        limit: usize,
    },
    Io(std::io::Error),
}

impl Error {
    pub(crate) fn parse(position: usize, message: impl Into<String>) -> Self {
        Self::Parse {
            position,
            message: message.into(),
        }
    }

    pub(crate) fn execution(message: impl Into<String>) -> Self {
        Self::Execution(message.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lex { position, message } => {
                write!(f, "lex error at byte {position}: {message}")
            }
            Self::Parse { position, message } => {
                write!(f, "parse error at byte {position}: {message}")
            }
            Self::Catalog(message) => write!(f, "catalog error: {message}"),
            Self::Type(message) => write!(f, "type error: {message}"),
            Self::Execution(message) => write!(f, "execution error: {message}"),
            Self::Limit { resource, limit } => {
                write!(
                    f,
                    "resource limit exceeded: {resource} is limited to {limit}"
                )
            }
            Self::Io(error) => write!(f, "I/O error: {error}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
