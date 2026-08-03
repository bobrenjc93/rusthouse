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

/// The typed syntax tree for a one-row, one-column `INSERT` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsertStatement {
    table_name: Identifier,
    value: Option<i64>,
}

impl InsertStatement {
    /// Returns the destination table name.
    pub fn table_name(&self) -> &Identifier {
        &self.table_name
    }

    /// Returns the statement's only value. `None` represents SQL `NULL`.
    pub fn value(&self) -> Option<i64> {
        self.value
    }
}

/// The typed syntax tree for a one-column `SELECT` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectStatement {
    column_name: Identifier,
    table_name: Identifier,
    limit: Option<usize>,
}

impl SelectStatement {
    /// Returns the projected column name.
    pub fn column_name(&self) -> &Identifier {
        &self.column_name
    }

    /// Returns the source table name.
    pub fn table_name(&self) -> &Identifier {
        &self.table_name
    }

    /// Returns the maximum number of rows requested by the statement.
    pub const fn limit(&self) -> Option<usize> {
        self.limit
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
    /// A value token is not `NULL` or a decimal `Int64` literal.
    InvalidInt64 { offset: usize },
    /// A syntactically valid decimal integer is outside the `Int64` range.
    Int64Overflow { offset: usize },
    /// A `LIMIT` value is not an unsigned decimal integer.
    InvalidLimit { offset: usize },
    /// A syntactically valid `LIMIT` value is outside the platform row-index range.
    LimitOverflow { offset: usize },
    /// Another row follows the single row supported by the grammar.
    ExtraRows { offset: usize },
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
            Self::InvalidInt64 { offset } => {
                write!(formatter, "invalid Int64 literal at byte {offset}")
            }
            Self::Int64Overflow { offset } => {
                write!(formatter, "Int64 literal at byte {offset} is out of range")
            }
            Self::InvalidLimit { offset } => {
                write!(formatter, "invalid LIMIT value at byte {offset}")
            }
            Self::LimitOverflow { offset } => {
                write!(formatter, "LIMIT value at byte {offset} is out of range")
            }
            Self::ExtraRows { offset } => {
                write!(
                    formatter,
                    "additional INSERT row at byte {offset} is not supported"
                )
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
    validate_statement_length(input, limits)?;

    Parser::new(input, limits.max_identifier_bytes).parse_create_table()
}

/// Parses one `INSERT INTO` statement containing one `Int64` or `NULL` value.
///
/// Keywords are ASCII case-insensitive. The table identifier follows the same
/// rules and bounds as [`parse_create_table`]. Decimal values may have a
/// leading `+` or `-`. The complete accepted grammar is:
///
/// ```text
/// INSERT INTO identifier VALUES (Int64 | NULL)
/// ```
///
/// Leading and trailing ASCII whitespace is accepted. Batches, multiple
/// values, and semicolon terminators are outside this deliberately narrow
/// grammar. Statement limits and all reported offsets are measured in bytes.
///
/// # Examples
///
/// ```
/// use rusthouse::{ParseLimits, parse_insert};
///
/// let statement = parse_insert(
///     "INSERT INTO events VALUES (-7)",
///     ParseLimits::default(),
/// )?;
///
/// assert_eq!(statement.table_name().as_str(), "events");
/// assert_eq!(statement.value(), Some(-7));
/// # Ok::<(), rusthouse::ParseError>(())
/// ```
pub fn parse_insert(input: &str, limits: ParseLimits) -> Result<InsertStatement, ParseError> {
    validate_statement_length(input, limits)?;

    Parser::new(input, limits.max_identifier_bytes).parse_insert()
}

/// Parses one `SELECT` statement containing one column and one table.
///
/// Keywords are ASCII case-insensitive. The column and table identifiers
/// follow the same rules and bounds as [`parse_create_table`]. The complete
/// accepted grammar is:
///
/// ```text
/// SELECT identifier FROM identifier [LIMIT unsigned-integer] [;]
/// ```
///
/// Leading and trailing ASCII whitespace is accepted, including whitespace
/// before or after the optional `LIMIT` and semicolon. `LIMIT` accepts zero and
/// must fit in `usize`. Statement limits and all reported offsets are measured
/// in bytes.
///
/// # Examples
///
/// ```
/// use rusthouse::{ParseLimits, parse_select};
///
/// let statement = parse_select(
///     "SELECT event_id FROM events LIMIT 10;",
///     ParseLimits::default(),
/// )?;
///
/// assert_eq!(statement.column_name().as_str(), "event_id");
/// assert_eq!(statement.table_name().as_str(), "events");
/// assert_eq!(statement.limit(), Some(10));
/// # Ok::<(), rusthouse::ParseError>(())
/// ```
pub fn parse_select(input: &str, limits: ParseLimits) -> Result<SelectStatement, ParseError> {
    validate_statement_length(input, limits)?;

    Parser::new(input, limits.max_identifier_bytes).parse_select()
}

fn validate_statement_length(input: &str, limits: ParseLimits) -> Result<(), ParseError> {
    if input.len() > limits.max_statement_bytes {
        return Err(ParseError::StatementTooLong {
            bytes: input.len(),
            max_bytes: limits.max_statement_bytes,
        });
    }

    Ok(())
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

    fn parse_create_table(mut self) -> Result<CreateTableStatement, ParseError> {
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

    fn parse_insert(mut self) -> Result<InsertStatement, ParseError> {
        self.skip_whitespace();
        self.expect_keyword("INSERT")?;
        self.require_whitespace("whitespace after INSERT")?;
        self.expect_keyword("INTO")?;
        self.require_whitespace("whitespace after INTO")?;
        let table_name = self.parse_identifier()?;
        self.require_whitespace("whitespace before VALUES")?;
        self.expect_keyword("VALUES")?;
        self.skip_whitespace();
        self.expect_byte(b'(', "'('")?;
        self.skip_whitespace();
        let value = self.parse_int64_value()?;
        self.skip_whitespace();

        match self.peek() {
            Some(b')') => self.position += 1,
            Some(_) => {
                return Err(ParseError::InvalidInt64 {
                    offset: self.position,
                });
            }
            None => return Err(self.unexpected("')'")),
        }
        self.skip_whitespace();

        if self.peek() == Some(b',') {
            return Err(ParseError::ExtraRows {
                offset: self.position,
            });
        }
        if self.position != self.bytes.len() {
            return Err(ParseError::TrailingInput {
                offset: self.position,
            });
        }

        Ok(InsertStatement { table_name, value })
    }

    fn parse_select(mut self) -> Result<SelectStatement, ParseError> {
        self.skip_whitespace();
        self.expect_keyword("SELECT")?;
        self.require_whitespace("whitespace after SELECT")?;
        if self.keyword_at_position("FROM") && !self.keyword_follows_current_word("FROM") {
            return Err(self.unexpected("identifier"));
        }
        let column_name = self.parse_identifier()?;
        self.require_whitespace("whitespace before FROM")?;
        self.expect_keyword("FROM")?;
        self.require_whitespace("whitespace after FROM")?;
        let table_name = self.parse_identifier()?;
        self.skip_whitespace();

        let limit = if self.keyword_at_position("LIMIT") {
            self.expect_keyword("LIMIT")?;
            self.require_whitespace("whitespace after LIMIT")?;
            let limit = self.parse_limit()?;
            self.skip_whitespace();
            Some(limit)
        } else {
            None
        };

        if self.peek() == Some(b';') {
            self.position += 1;
            self.skip_whitespace();
        }
        if self.position != self.bytes.len() {
            return Err(ParseError::TrailingInput {
                offset: self.position,
            });
        }

        Ok(SelectStatement {
            column_name,
            table_name,
            limit,
        })
    }

    fn parse_limit(&mut self) -> Result<usize, ParseError> {
        let start = self.position;
        while self
            .peek()
            .is_some_and(|byte| !byte.is_ascii_whitespace() && byte != b';')
        {
            self.position += 1;
        }

        let literal = &self.input[start..self.position];
        if literal.is_empty() || !literal.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(ParseError::InvalidLimit { offset: start });
        }

        literal
            .parse()
            .map_err(|_| ParseError::LimitOverflow { offset: start })
    }

    fn parse_int64_value(&mut self) -> Result<Option<i64>, ParseError> {
        let start = self.position;
        if self.keyword_at_position("NULL") {
            self.expect_keyword("NULL")?;
            return Ok(None);
        }

        if matches!(self.peek(), Some(b'+' | b'-')) {
            self.position += 1;
        }
        let digits_start = self.position;
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.position += 1;
        }

        if self.position == digits_start {
            return Err(ParseError::InvalidInt64 { offset: start });
        }

        let end = self.position;
        self.skip_whitespace();
        match self.peek() {
            Some(b')') => {}
            Some(_) => {
                return Err(ParseError::InvalidInt64 {
                    offset: self.position,
                });
            }
            None => return Err(self.unexpected("')'")),
        }

        let literal = &self.input[start..end];
        literal
            .parse::<i64>()
            .map(Some)
            .map_err(|_| ParseError::Int64Overflow { offset: start })
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

    fn keyword_follows_current_word(&self, keyword: &str) -> bool {
        let Some(current_end) = self.word_end(self.position) else {
            return false;
        };

        let mut next_start = current_end;
        while self
            .bytes
            .get(next_start)
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            next_start += 1;
        }
        if next_start == current_end {
            return false;
        }

        self.word_end(next_start)
            .is_some_and(|next_end| self.input[next_start..next_end].eq_ignore_ascii_case(keyword))
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
