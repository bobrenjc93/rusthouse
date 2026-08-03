//! Parsing and syntax tree types for RustHouse's initial SQL boundary.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;

pub use crate::storage::DataType;

/// One named, typed column in a table declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDefinition {
    pub name: String,
    pub data_type: DataType,
}

/// The syntax tree produced for a `CREATE TABLE` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateTableStatement {
    pub name: String,
    pub columns: Vec<ColumnDefinition>,
}

/// Resource limits applied before and during parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseLimits {
    pub max_input_bytes: usize,
    pub max_columns: usize,
}

impl ParseLimits {
    pub const DEFAULT_MAX_INPUT_BYTES: usize = 1024 * 1024;
    pub const DEFAULT_MAX_COLUMNS: usize = 1024;

    pub const fn new(max_input_bytes: usize, max_columns: usize) -> Self {
        Self {
            max_input_bytes,
            max_columns,
        }
    }
}

impl Default for ParseLimits {
    fn default() -> Self {
        Self::new(Self::DEFAULT_MAX_INPUT_BYTES, Self::DEFAULT_MAX_COLUMNS)
    }
}

/// The role of an identifier which failed validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentifierContext {
    Table,
    Column,
}

impl fmt::Display for IdentifierContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Table => formatter.write_str("table"),
            Self::Column => formatter.write_str("column"),
        }
    }
}

/// A specific reason that a `CREATE TABLE` statement could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseErrorKind {
    InputTooLong {
        limit: usize,
        actual: usize,
    },
    ExpectedKeyword {
        expected: &'static str,
        found: Option<String>,
    },
    ExpectedIdentifier {
        context: IdentifierContext,
    },
    InvalidIdentifier {
        context: IdentifierContext,
        identifier: String,
    },
    ExpectedToken {
        expected: &'static str,
    },
    EmptyColumn,
    DuplicateColumn {
        name: String,
        first_position: usize,
    },
    ExpectedType,
    UnknownType {
        type_name: String,
    },
    TooManyColumns {
        limit: usize,
    },
    TrailingSyntax,
}

impl fmt::Display for ParseErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputTooLong { limit, actual } => {
                write!(formatter, "input is {actual} bytes; limit is {limit}")
            }
            Self::ExpectedKeyword { expected, found } => match found {
                Some(found) => write!(formatter, "expected keyword {expected}, found {found:?}"),
                None => write!(formatter, "expected keyword {expected}"),
            },
            Self::ExpectedIdentifier { context } => {
                write!(formatter, "expected {context} identifier")
            }
            Self::InvalidIdentifier {
                context,
                identifier,
            } => write!(formatter, "invalid {context} identifier {identifier:?}"),
            Self::ExpectedToken { expected } => write!(formatter, "expected {expected}"),
            Self::EmptyColumn => formatter.write_str("column declaration is empty"),
            Self::DuplicateColumn {
                name,
                first_position,
            } => write!(
                formatter,
                "duplicate column {name:?}; first declared at byte {first_position}"
            ),
            Self::ExpectedType => formatter.write_str("expected column type"),
            Self::UnknownType { type_name } => {
                write!(formatter, "unknown column type {type_name:?}")
            }
            Self::TooManyColumns { limit } => {
                write!(formatter, "column count exceeds limit of {limit}")
            }
            Self::TrailingSyntax => formatter.write_str("trailing syntax after statement"),
        }
    }
}

/// A parse error and the zero-based byte position at which it was detected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub position: usize,
    pub kind: ParseErrorKind,
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "SQL parse error at byte {}: {}",
            self.position, self.kind
        )
    }
}

impl Error for ParseError {}

/// Parses one bounded `CREATE TABLE` statement using the default limits.
pub fn parse_create_table(input: &str) -> Result<CreateTableStatement, ParseError> {
    parse_create_table_with_limits(input, ParseLimits::default())
}

/// Parses one bounded `CREATE TABLE` statement.
///
/// Keywords and data types are case-insensitive. Unquoted identifiers must match
/// `[A-Za-z_][A-Za-z0-9_]*`. One trailing semicolon is accepted as a statement
/// terminator, but comments, quoted identifiers, and additional statements are
/// outside this parser's intentionally narrow SQL surface.
pub fn parse_create_table_with_limits(
    input: &str,
    limits: ParseLimits,
) -> Result<CreateTableStatement, ParseError> {
    if input.len() > limits.max_input_bytes {
        return Err(ParseError {
            position: limits.max_input_bytes,
            kind: ParseErrorKind::InputTooLong {
                limit: limits.max_input_bytes,
                actual: input.len(),
            },
        });
    }

    Parser::new(input, limits.max_columns).parse()
}

struct Parser<'a> {
    input: &'a str,
    position: usize,
    max_columns: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str, max_columns: usize) -> Self {
        Self {
            input,
            position: 0,
            max_columns,
        }
    }

    fn parse(mut self) -> Result<CreateTableStatement, ParseError> {
        self.parse_keyword("CREATE")?;
        self.parse_keyword("TABLE")?;
        let (name, _) = self.parse_identifier(IdentifierContext::Table)?;
        self.expect_byte(b'(', "'('")?;

        let mut columns = Vec::new();
        let mut column_positions = HashMap::new();

        loop {
            self.skip_whitespace();
            if self.peek().is_none() || matches!(self.peek(), Some(b')' | b',')) {
                return Err(self.error(ParseErrorKind::EmptyColumn));
            }
            if columns.len() == self.max_columns {
                return Err(self.error(ParseErrorKind::TooManyColumns {
                    limit: self.max_columns,
                }));
            }

            let (column_name, column_position) =
                self.parse_identifier(IdentifierContext::Column)?;
            let normalized_name = column_name.to_ascii_lowercase();
            if let Some(first_position) = column_positions.get(&normalized_name) {
                return Err(ParseError {
                    position: column_position,
                    kind: ParseErrorKind::DuplicateColumn {
                        name: column_name,
                        first_position: *first_position,
                    },
                });
            }

            let data_type = self.parse_data_type()?;
            column_positions.insert(normalized_name, column_position);
            columns.push(ColumnDefinition {
                name: column_name,
                data_type,
            });

            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.position += 1,
                Some(b')') => {
                    self.position += 1;
                    break;
                }
                _ => {
                    return Err(self.error(ParseErrorKind::ExpectedToken {
                        expected: "',' or ')'",
                    }));
                }
            }
        }

        self.skip_whitespace();
        if self.peek() == Some(b';') {
            self.position += 1;
            self.skip_whitespace();
        }
        if self.peek().is_some() {
            return Err(self.error(ParseErrorKind::TrailingSyntax));
        }

        Ok(CreateTableStatement { name, columns })
    }

    fn parse_keyword(&mut self, expected: &'static str) -> Result<(), ParseError> {
        self.skip_whitespace();
        let start = self.position;
        let found = self.take_token();
        if found.eq_ignore_ascii_case(expected) {
            Ok(())
        } else {
            Err(ParseError {
                position: start,
                kind: ParseErrorKind::ExpectedKeyword {
                    expected,
                    found: (!found.is_empty()).then(|| found.to_owned()),
                },
            })
        }
    }

    fn parse_identifier(
        &mut self,
        context: IdentifierContext,
    ) -> Result<(String, usize), ParseError> {
        self.skip_whitespace();
        let start = self.position;
        let identifier = self.take_token();
        if identifier.is_empty() {
            return Err(ParseError {
                position: start,
                kind: ParseErrorKind::ExpectedIdentifier { context },
            });
        }

        if let Some(offset) = invalid_identifier_offset(identifier) {
            return Err(ParseError {
                position: start + offset,
                kind: ParseErrorKind::InvalidIdentifier {
                    context,
                    identifier: identifier.to_owned(),
                },
            });
        }

        Ok((identifier.to_owned(), start))
    }

    fn parse_data_type(&mut self) -> Result<DataType, ParseError> {
        self.skip_whitespace();
        let start = self.position;
        let type_name = self.take_token();
        if type_name.is_empty() {
            return Err(ParseError {
                position: start,
                kind: ParseErrorKind::ExpectedType,
            });
        }

        if type_name.eq_ignore_ascii_case("Int64") {
            Ok(DataType::Int64)
        } else if type_name.eq_ignore_ascii_case("Float64") {
            Ok(DataType::Float64)
        } else if type_name.eq_ignore_ascii_case("Bool") {
            Ok(DataType::Bool)
        } else if type_name.eq_ignore_ascii_case("String") {
            Ok(DataType::String)
        } else {
            Err(ParseError {
                position: start,
                kind: ParseErrorKind::UnknownType {
                    type_name: type_name.to_owned(),
                },
            })
        }
    }

    fn expect_byte(&mut self, byte: u8, expected: &'static str) -> Result<(), ParseError> {
        self.skip_whitespace();
        if self.peek() == Some(byte) {
            self.position += 1;
            Ok(())
        } else {
            Err(self.error(ParseErrorKind::ExpectedToken { expected }))
        }
    }

    fn take_token(&mut self) -> &'a str {
        let start = self.position;
        while let Some(byte) = self.peek() {
            if is_whitespace(byte) || matches!(byte, b'(' | b')' | b',' | b';') {
                break;
            }
            self.position += 1;
        }
        &self.input[start..self.position]
    }

    fn skip_whitespace(&mut self) {
        while self.peek().is_some_and(is_whitespace) {
            self.position += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.as_bytes().get(self.position).copied()
    }

    fn error(&self, kind: ParseErrorKind) -> ParseError {
        ParseError {
            position: self.position,
            kind,
        }
    }
}

fn invalid_identifier_offset(identifier: &str) -> Option<usize> {
    let bytes = identifier.as_bytes();
    if !bytes
        .first()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
    {
        return Some(0);
    }

    bytes
        .iter()
        .position(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_')
}

fn is_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}
