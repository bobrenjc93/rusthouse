//! A deliberately small, bounded SQL parser.

use std::error::Error;
use std::fmt;

use crate::storage::DataType;

/// Default maximum size of a SQL statement, in bytes.
pub const DEFAULT_MAX_STATEMENT_BYTES: usize = 4 * 1024;

/// Default maximum size of an identifier, in bytes.
pub const DEFAULT_MAX_IDENTIFIER_BYTES: usize = 128;

/// Resource bounds applied before and while parsing SQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseLimits {
    /// Maximum number of bytes allowed in the complete input.
    pub max_statement_bytes: usize,
    /// Maximum number of bytes allowed in each identifier.
    pub max_identifier_bytes: usize,
}

impl ParseLimits {
    /// Creates explicit statement and identifier bounds.
    pub const fn new(max_statement_bytes: usize, max_identifier_bytes: usize) -> Self {
        Self {
            max_statement_bytes,
            max_identifier_bytes,
        }
    }
}

impl Default for ParseLimits {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_STATEMENT_BYTES, DEFAULT_MAX_IDENTIFIER_BYTES)
    }
}

/// An unquoted SQL identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Identifier(String);

impl Identifier {
    /// Returns the identifier exactly as it appeared in the statement.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for Identifier {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for Identifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A typed column declaration in a `CREATE TABLE` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDefinition {
    name: Identifier,
    data_type: DataType,
    nullable: bool,
}

impl ColumnDefinition {
    /// Returns the column name.
    pub fn name(&self) -> &Identifier {
        &self.name
    }

    /// Returns the declared logical type.
    pub fn data_type(&self) -> DataType {
        self.data_type
    }

    /// Returns whether the column accepts `NULL` values.
    pub fn is_nullable(&self) -> bool {
        self.nullable
    }
}

/// The typed syntax tree for a one-column `CREATE TABLE` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateTableStatement {
    table_name: Identifier,
    column: ColumnDefinition,
}

impl CreateTableStatement {
    /// Returns the table name.
    pub fn table_name(&self) -> &Identifier {
        &self.table_name
    }

    /// Returns the statement's only column definition.
    pub fn column(&self) -> &ColumnDefinition {
        &self.column
    }
}

/// An error produced while parsing a bounded SQL statement.
///
/// Offsets and sizes are byte-based, matching Rust string indexing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// The complete input exceeds the configured statement bound.
    StatementTooLong { bytes: usize, max_bytes: usize },
    /// An identifier exceeds the configured identifier bound.
    IdentifierTooLong {
        offset: usize,
        bytes: usize,
        max_bytes: usize,
    },
    /// The input did not contain the required grammar element.
    UnexpectedInput {
        offset: usize,
        expected: &'static str,
    },
    /// Non-whitespace input remained after the closing parenthesis.
    TrailingInput { offset: usize },
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StatementTooLong { bytes, max_bytes } => write!(
                formatter,
                "statement is {bytes} bytes, exceeding the limit of {max_bytes} bytes"
            ),
            Self::IdentifierTooLong {
                offset,
                bytes,
                max_bytes,
            } => write!(
                formatter,
                "identifier at byte {offset} is {bytes} bytes, exceeding the limit of {max_bytes} bytes"
            ),
            Self::UnexpectedInput { offset, expected } => {
                write!(formatter, "expected {expected} at byte {offset}")
            }
            Self::TrailingInput { offset } => {
                write!(formatter, "unexpected trailing input at byte {offset}")
            }
        }
    }
}

impl Error for ParseError {}

/// Parses one `CREATE TABLE` statement with one `Int64` column.
///
/// Keywords are ASCII case-insensitive. Identifiers must match
/// `[A-Za-z_][A-Za-z0-9_]*` and retain their original spelling. An omitted
/// nullability clause has SQL's default nullable behavior. The complete
/// accepted grammar is:
///
/// ```text
/// CREATE TABLE identifier (identifier Int64 [NULL | NOT NULL])
/// ```
///
/// Leading and trailing ASCII whitespace is accepted. Statement and
/// identifier limits, along with every reported offset, are measured in
/// bytes.
///
/// # Examples
///
/// ```
/// use rusthouse::{DataType, ParseLimits, parse_create_table};
///
/// let statement = parse_create_table(
///     "CREATE TABLE events (event_id Int64 NOT NULL)",
///     ParseLimits::default(),
/// )?;
///
/// assert_eq!(statement.table_name().as_str(), "events");
/// assert_eq!(statement.column().name().as_str(), "event_id");
/// assert_eq!(statement.column().data_type(), DataType::Int64);
/// assert!(!statement.column().is_nullable());
/// # Ok::<(), rusthouse::ParseError>(())
/// ```
pub fn parse_create_table(
    input: &str,
    limits: ParseLimits,
) -> Result<CreateTableStatement, ParseError> {
    if input.len() > limits.max_statement_bytes {
        return Err(ParseError::StatementTooLong {
            bytes: input.len(),
            max_bytes: limits.max_statement_bytes,
        });
    }

    Parser::new(input, limits.max_identifier_bytes).parse()
}

struct Parser<'input> {
    input: &'input str,
    bytes: &'input [u8],
    position: usize,
    max_identifier_bytes: usize,
}

impl<'input> Parser<'input> {
    fn new(input: &'input str, max_identifier_bytes: usize) -> Self {
        Self {
            input,
            bytes: input.as_bytes(),
            position: 0,
            max_identifier_bytes,
        }
    }

    fn parse(mut self) -> Result<CreateTableStatement, ParseError> {
        self.skip_whitespace();
        self.expect_keyword("CREATE")?;
        self.require_whitespace("whitespace after CREATE")?;
        self.expect_keyword("TABLE")?;
        self.require_whitespace("whitespace after TABLE")?;
        let table_name = self.parse_identifier()?;

        self.skip_whitespace();
        self.expect_byte(b'(', "'('")?;
        self.skip_whitespace();
        let column_name = self.parse_identifier()?;
        self.require_whitespace("whitespace before Int64")?;
        self.expect_keyword("Int64")?;

        let nullable = self.parse_nullability()?;
        self.skip_whitespace();
        self.expect_byte(b')', "')'")?;
        self.skip_whitespace();

        if self.position != self.bytes.len() {
            return Err(ParseError::TrailingInput {
                offset: self.position,
            });
        }

        Ok(CreateTableStatement {
            table_name,
            column: ColumnDefinition {
                name: column_name,
                data_type: DataType::Int64,
                nullable,
            },
        })
    }

    fn parse_nullability(&mut self) -> Result<bool, ParseError> {
        self.skip_whitespace();

        if self.peek() == Some(b')') {
            return Ok(true);
        }

        if self.keyword_at_position("NULL") {
            self.expect_keyword("NULL")?;
            return Ok(true);
        }

        if self.keyword_at_position("NOT") {
            self.expect_keyword("NOT")?;
            self.require_whitespace("whitespace after NOT")?;
            self.expect_keyword("NULL")?;
            return Ok(false);
        }

        Err(self.unexpected("NULL, NOT NULL, or ')'"))
    }

    fn parse_identifier(&mut self) -> Result<Identifier, ParseError> {
        let start = self.position;
        if !self.peek().is_some_and(is_identifier_start) {
            return Err(self.unexpected("identifier"));
        }

        self.position += 1;
        while self.peek().is_some_and(is_identifier_continue) {
            self.position += 1;
        }

        let bytes = self.position - start;
        if bytes > self.max_identifier_bytes {
            return Err(ParseError::IdentifierTooLong {
                offset: start,
                bytes,
                max_bytes: self.max_identifier_bytes,
            });
        }

        Ok(Identifier(self.input[start..self.position].to_owned()))
    }

    fn expect_keyword(&mut self, keyword: &'static str) -> Result<(), ParseError> {
        let start = self.position;
        let Some(end) = self.word_end(start) else {
            return Err(self.unexpected(keyword));
        };

        if !self.input[start..end].eq_ignore_ascii_case(keyword) {
            return Err(self.unexpected(keyword));
        }

        self.position = end;
        Ok(())
    }

    fn keyword_at_position(&self, keyword: &str) -> bool {
        self.word_end(self.position)
            .is_some_and(|end| self.input[self.position..end].eq_ignore_ascii_case(keyword))
    }

    fn word_end(&self, start: usize) -> Option<usize> {
        if !self
            .bytes
            .get(start)
            .copied()
            .is_some_and(is_identifier_start)
        {
            return None;
        }

        let mut end = start + 1;
        while self
            .bytes
            .get(end)
            .copied()
            .is_some_and(is_identifier_continue)
        {
            end += 1;
        }
        Some(end)
    }

    fn require_whitespace(&mut self, expected: &'static str) -> Result<(), ParseError> {
        let start = self.position;
        self.skip_whitespace();
        if self.position == start {
            return Err(self.unexpected(expected));
        }
        Ok(())
    }

    fn skip_whitespace(&mut self) {
        while self.peek().is_some_and(|byte| byte.is_ascii_whitespace()) {
            self.position += 1;
        }
    }

    fn expect_byte(&mut self, byte: u8, expected: &'static str) -> Result<(), ParseError> {
        if self.peek() != Some(byte) {
            return Err(self.unexpected(expected));
        }
        self.position += 1;
        Ok(())
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }

    fn unexpected(&self, expected: &'static str) -> ParseError {
        ParseError::UnexpectedInput {
            offset: self.position,
            expected,
        }
    }
}

fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_identifier_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}
