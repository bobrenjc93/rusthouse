//! Parsing for RustHouse's bounded SQL surface.

use std::error::Error;
use std::fmt;

pub use crate::{DataType, Value};

/// Maximum SQL statement size accepted by [`parse_create_table`], in bytes.
pub const MAX_SQL_BYTES: usize = 64 * 1024;

/// Maximum number of columns accepted by [`parse_create_table`].
pub const MAX_COLUMNS: usize = 1_024;

/// Maximum number of rows accepted by [`parse_insert`].
pub const MAX_INSERT_ROWS: usize = 10_000;

/// Maximum total number of values accepted by [`parse_insert`].
pub const MAX_INSERT_VALUES: usize = 16_384;

/// Maximum decoded String payload accepted by [`parse_insert`], in bytes.
pub const MAX_INSERT_STRING_BYTES: usize = 32 * 1024;

/// Resource limits applied while parsing a SQL statement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParseLimits {
    pub max_sql_bytes: usize,
    pub max_columns: usize,
}

impl Default for ParseLimits {
    fn default() -> Self {
        Self {
            max_sql_bytes: MAX_SQL_BYTES,
            max_columns: MAX_COLUMNS,
        }
    }
}

/// Resource limits applied while parsing an INSERT statement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InsertParseLimits {
    pub max_sql_bytes: usize,
    pub max_rows: usize,
    pub max_values: usize,
    pub max_string_bytes: usize,
}

impl Default for InsertParseLimits {
    fn default() -> Self {
        Self {
            max_sql_bytes: MAX_SQL_BYTES,
            max_rows: MAX_INSERT_ROWS,
            max_values: MAX_INSERT_VALUES,
            max_string_bytes: MAX_INSERT_STRING_BYTES,
        }
    }
}

/// One named, typed column in a `CREATE TABLE` statement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ColumnDefinition {
    pub name: String,
    pub data_type: DataType,
}

/// The typed result of parsing one `CREATE TABLE` statement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateTableStatement {
    pub table_name: String,
    /// Columns in the order in which they appeared in the statement.
    pub columns: Vec<ColumnDefinition>,
}

/// The typed result of parsing one `INSERT INTO ... VALUES` statement.
///
/// `rows` has the same representation accepted by
/// [`crate::Table::insert_batch`].
#[derive(Clone, Debug, PartialEq)]
pub struct InsertStatement {
    pub table_name: String,
    /// Rows and their values in statement order.
    pub rows: Vec<Vec<Value>>,
}

/// A typed SQL parse failure.
///
/// Positions are zero-based byte offsets into the original SQL string. At end
/// of input, the position equals the string's byte length.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParseError {
    /// The input did not match the supported grammar.
    Syntax {
        position: usize,
        expected: &'static str,
        found: Option<String>,
    },
    /// A syntactically valid type name is not supported.
    UnsupportedType { position: usize, type_name: String },
    /// Non-whitespace input followed the statement or its optional semicolon.
    TrailingInput { position: usize },
    /// The input exceeded the configured byte limit.
    SqlTooLarge {
        position: usize,
        max_bytes: usize,
        actual_bytes: usize,
    },
    /// The statement exceeded the configured column limit.
    TooManyColumns { position: usize, max_columns: usize },
    /// An INSERT exceeded the configured row limit.
    TooManyRows { position: usize, max_rows: usize },
    /// An INSERT exceeded the configured total value limit.
    TooManyValues { position: usize, max_values: usize },
    /// String literals exceeded the configured decoded UTF-8 byte limit.
    StringByteLimitExceeded {
        position: usize,
        max_bytes: usize,
        attempted_bytes: usize,
    },
    /// An integer literal was outside the `i64` range.
    IntegerOverflow { position: usize, literal: String },
    /// A floating-point literal evaluated to a non-finite value.
    NonFiniteFloat { position: usize, literal: String },
}

impl ParseError {
    /// Returns the zero-based byte offset associated with this error.
    pub const fn position(&self) -> usize {
        match self {
            Self::Syntax { position, .. }
            | Self::UnsupportedType { position, .. }
            | Self::TrailingInput { position }
            | Self::SqlTooLarge { position, .. }
            | Self::TooManyColumns { position, .. }
            | Self::TooManyRows { position, .. }
            | Self::TooManyValues { position, .. }
            | Self::StringByteLimitExceeded { position, .. }
            | Self::IntegerOverflow { position, .. }
            | Self::NonFiniteFloat { position, .. } => *position,
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax {
                position,
                expected,
                found,
            } => match found {
                Some(found) => write!(
                    formatter,
                    "expected {expected} at byte {position}, found {found:?}"
                ),
                None => write!(
                    formatter,
                    "expected {expected} at byte {position}, found end of input"
                ),
            },
            Self::UnsupportedType {
                position,
                type_name,
            } => write!(
                formatter,
                "unsupported type {type_name:?} at byte {position}"
            ),
            Self::TrailingInput { position } => {
                write!(formatter, "trailing input at byte {position}")
            }
            Self::SqlTooLarge {
                position,
                max_bytes,
                actual_bytes,
            } => write!(
                formatter,
                "SQL is {actual_bytes} bytes, exceeding the {max_bytes}-byte limit at byte {position}"
            ),
            Self::TooManyColumns {
                position,
                max_columns,
            } => write!(
                formatter,
                "column at byte {position} exceeds the {max_columns}-column limit"
            ),
            Self::TooManyRows { position, max_rows } => write!(
                formatter,
                "row at byte {position} exceeds the {max_rows}-row limit"
            ),
            Self::TooManyValues {
                position,
                max_values,
            } => write!(
                formatter,
                "value at byte {position} exceeds the {max_values}-value limit"
            ),
            Self::StringByteLimitExceeded {
                position,
                max_bytes,
                attempted_bytes,
            } => write!(
                formatter,
                "String literals total {attempted_bytes} bytes, exceeding the {max_bytes}-byte limit at byte {position}"
            ),
            Self::IntegerOverflow { position, literal } => write!(
                formatter,
                "integer literal {literal:?} overflows Int64 at byte {position}"
            ),
            Self::NonFiniteFloat { position, literal } => write!(
                formatter,
                "floating-point literal {literal:?} is non-finite at byte {position}"
            ),
        }
    }
}

impl Error for ParseError {}

/// Parses one bounded `CREATE TABLE` statement using the default limits.
///
/// Keywords and the four supported data types are ASCII case-insensitive.
/// Identifiers consist of an ASCII letter or underscore followed by ASCII
/// letters, digits, or underscores. One optional trailing semicolon is
/// accepted.
///
/// # Examples
///
/// ```
/// use rusthouse::sql::{DataType, parse_create_table};
///
/// let statement = parse_create_table(
///     "CREATE TABLE readings (time Int64, value Float64, valid Bool, tag String);",
/// )?;
/// assert_eq!(statement.table_name, "readings");
/// assert_eq!(statement.columns[1].data_type, DataType::Float64);
/// # Ok::<(), rusthouse::sql::ParseError>(())
/// ```
pub fn parse_create_table(sql: &str) -> Result<CreateTableStatement, ParseError> {
    parse_create_table_with_limits(sql, ParseLimits::default())
}

/// Parses one `CREATE TABLE` statement using caller-provided resource limits.
pub fn parse_create_table_with_limits(
    sql: &str,
    limits: ParseLimits,
) -> Result<CreateTableStatement, ParseError> {
    enforce_sql_size(sql, limits.max_sql_bytes)?;

    Parser::new(sql, limits.max_columns).parse()
}

/// Parses one bounded `INSERT INTO ... VALUES` statement using the default
/// limits.
///
/// Integer literals become [`Value::Int64`], while literals containing a
/// decimal point or exponent become [`Value::Float64`]. Boolean keywords are
/// ASCII case-insensitive. String literals use single quotes and escape a
/// quote by doubling it.
///
/// # Examples
///
/// ```
/// use rusthouse::sql::{Value, parse_insert};
///
/// let statement = parse_insert(
///     "INSERT INTO readings VALUES (-2, 1.5, TRUE, 'it''s ready'), (3, -4e-1, false, 'next');",
/// )?;
/// assert_eq!(statement.table_name, "readings");
/// assert_eq!(statement.rows.len(), 2);
/// assert_eq!(statement.rows[0][0], Value::Int64(-2));
/// assert_eq!(statement.rows[0][3], Value::String("it's ready".into()));
/// # Ok::<(), rusthouse::sql::ParseError>(())
/// ```
pub fn parse_insert(sql: &str) -> Result<InsertStatement, ParseError> {
    parse_insert_with_limits(sql, InsertParseLimits::default())
}

/// Parses one `INSERT INTO ... VALUES` statement using caller-provided
/// resource limits.
pub fn parse_insert_with_limits(
    sql: &str,
    limits: InsertParseLimits,
) -> Result<InsertStatement, ParseError> {
    enforce_sql_size(sql, limits.max_sql_bytes)?;
    InsertParser::new(sql, limits).parse()
}

fn enforce_sql_size(sql: &str, max_bytes: usize) -> Result<(), ParseError> {
    if sql.len() > max_bytes {
        return Err(ParseError::SqlTooLarge {
            position: max_bytes,
            max_bytes,
            actual_bytes: sql.len(),
        });
    }

    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TokenKind {
    Word,
    LeftParenthesis,
    RightParenthesis,
    Comma,
    Semicolon,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Token {
    kind: TokenKind,
    start: usize,
    end: usize,
}

struct Lexer<'a> {
    sql: &'a str,
    position: usize,
}

impl<'a> Lexer<'a> {
    fn new(sql: &'a str) -> Self {
        Self { sql, position: 0 }
    }

    fn next(&mut self) -> Option<Token> {
        while let Some(character) = self.current_character() {
            if !character.is_ascii_whitespace() {
                break;
            }
            self.position += character.len_utf8();
        }

        let start = self.position;
        let character = self.current_character()?;
        let kind = match character {
            '(' => TokenKind::LeftParenthesis,
            ')' => TokenKind::RightParenthesis,
            ',' => TokenKind::Comma,
            ';' => TokenKind::Semicolon,
            character if is_identifier_start(character) => {
                self.position += character.len_utf8();
                while let Some(character) = self.current_character() {
                    if !is_identifier_continue(character) {
                        break;
                    }
                    self.position += character.len_utf8();
                }
                return Some(Token {
                    kind: TokenKind::Word,
                    start,
                    end: self.position,
                });
            }
            _ => TokenKind::Other,
        };

        self.position += character.len_utf8();
        Some(Token {
            kind,
            start,
            end: self.position,
        })
    }

    fn current_character(&self) -> Option<char> {
        self.sql[self.position..].chars().next()
    }
}

const fn is_identifier_start(character: char) -> bool {
    character.is_ascii_alphabetic() || character == '_'
}

const fn is_identifier_continue(character: char) -> bool {
    is_identifier_start(character) || character.is_ascii_digit()
}

struct Parser<'a> {
    sql: &'a str,
    lexer: Lexer<'a>,
    lookahead: Option<Option<Token>>,
    max_columns: usize,
}

impl<'a> Parser<'a> {
    fn new(sql: &'a str, max_columns: usize) -> Self {
        Self {
            sql,
            lexer: Lexer::new(sql),
            lookahead: None,
            max_columns,
        }
    }

    fn parse(mut self) -> Result<CreateTableStatement, ParseError> {
        self.expect_keyword("CREATE")?;
        self.expect_keyword("TABLE")?;
        let table_name = self.expect_identifier("table name")?;
        self.expect_kind(TokenKind::LeftParenthesis, "'('")?;

        let mut columns = Vec::with_capacity(self.max_columns.min(16));
        loop {
            let column_token = self.expect_word("column name")?;
            if columns.len() == self.max_columns {
                return Err(ParseError::TooManyColumns {
                    position: column_token.start,
                    max_columns: self.max_columns,
                });
            }

            let column_name = self.token_text(column_token).to_owned();
            let type_token = self.expect_word("column type")?;
            let type_name = self.token_text(type_token);
            let data_type =
                parse_data_type(type_name).ok_or_else(|| ParseError::UnsupportedType {
                    position: type_token.start,
                    type_name: type_name.to_owned(),
                })?;
            columns.push(ColumnDefinition {
                name: column_name,
                data_type,
            });

            match self.peek() {
                Some(token) if token.kind == TokenKind::Comma => {
                    self.next();
                }
                Some(token) if token.kind == TokenKind::RightParenthesis => {
                    self.next();
                    break;
                }
                token => return Err(self.syntax_error("',' or ')'", token)),
            }
        }

        match self.next() {
            None => {}
            Some(token) if token.kind == TokenKind::Semicolon => {
                if let Some(trailing) = self.next() {
                    return Err(ParseError::TrailingInput {
                        position: trailing.start,
                    });
                }
            }
            Some(token) => {
                return Err(ParseError::TrailingInput {
                    position: token.start,
                });
            }
        }

        Ok(CreateTableStatement {
            table_name,
            columns,
        })
    }

    fn expect_keyword(&mut self, keyword: &'static str) -> Result<(), ParseError> {
        let token = self.next();
        match token {
            Some(token)
                if token.kind == TokenKind::Word
                    && self.token_text(token).eq_ignore_ascii_case(keyword) =>
            {
                Ok(())
            }
            token => Err(self.syntax_error(keyword, token)),
        }
    }

    fn expect_identifier(&mut self, expected: &'static str) -> Result<String, ParseError> {
        self.expect_word(expected)
            .map(|token| self.token_text(token).to_owned())
    }

    fn expect_word(&mut self, expected: &'static str) -> Result<Token, ParseError> {
        let token = self.next();
        match token {
            Some(token) if token.kind == TokenKind::Word => Ok(token),
            token => Err(self.syntax_error(expected, token)),
        }
    }

    fn expect_kind(
        &mut self,
        kind: TokenKind,
        expected: &'static str,
    ) -> Result<Token, ParseError> {
        let token = self.next();
        match token {
            Some(token) if token.kind == kind => Ok(token),
            token => Err(self.syntax_error(expected, token)),
        }
    }

    fn peek(&mut self) -> Option<Token> {
        if self.lookahead.is_none() {
            self.lookahead = Some(self.lexer.next());
        }
        self.lookahead.flatten()
    }

    fn next(&mut self) -> Option<Token> {
        self.lookahead.take().unwrap_or_else(|| self.lexer.next())
    }

    fn token_text(&self, token: Token) -> &'a str {
        &self.sql[token.start..token.end]
    }

    fn syntax_error(&self, expected: &'static str, token: Option<Token>) -> ParseError {
        ParseError::Syntax {
            position: token.map_or(self.sql.len(), |token| token.start),
            expected,
            found: token.map(|token| self.token_text(token).to_owned()),
        }
    }
}

fn parse_data_type(type_name: &str) -> Option<DataType> {
    if type_name.eq_ignore_ascii_case("Int64") {
        Some(DataType::Int64)
    } else if type_name.eq_ignore_ascii_case("Float64") {
        Some(DataType::Float64)
    } else if type_name.eq_ignore_ascii_case("Bool") {
        Some(DataType::Bool)
    } else if type_name.eq_ignore_ascii_case("String") {
        Some(DataType::String)
    } else {
        None
    }
}

struct InsertParser<'a> {
    sql: &'a str,
    position: usize,
    limits: InsertParseLimits,
    value_count: usize,
    string_bytes: usize,
}

impl<'a> InsertParser<'a> {
    fn new(sql: &'a str, limits: InsertParseLimits) -> Self {
        Self {
            sql,
            position: 0,
            limits,
            value_count: 0,
            string_bytes: 0,
        }
    }

    fn parse(mut self) -> Result<InsertStatement, ParseError> {
        self.expect_keyword("INSERT")?;
        self.expect_keyword("INTO")?;
        let table_name = self.expect_identifier("table name")?;
        self.expect_keyword("VALUES")?;

        let mut rows = Vec::with_capacity(self.limits.max_rows.min(16));
        loop {
            self.skip_whitespace();
            let row_position = self.position;
            self.expect_byte(b'(', "'('")?;
            if rows.len() == self.limits.max_rows {
                return Err(ParseError::TooManyRows {
                    position: row_position,
                    max_rows: self.limits.max_rows,
                });
            }

            let mut row = Vec::new();
            loop {
                self.skip_whitespace();
                let value_position = self.position;
                if self.value_count == self.limits.max_values {
                    return Err(ParseError::TooManyValues {
                        position: value_position,
                        max_values: self.limits.max_values,
                    });
                }

                row.push(self.parse_value()?);
                self.value_count += 1;

                self.skip_whitespace();
                match self.current_byte() {
                    Some(b',') => self.position += 1,
                    Some(b')') => {
                        self.position += 1;
                        break;
                    }
                    _ => return Err(self.syntax_error("',' or ')'")),
                }
            }
            rows.push(row);

            self.skip_whitespace();
            if self.current_byte() == Some(b',') {
                self.position += 1;
            } else {
                break;
            }
        }

        self.finish_statement()?;
        Ok(InsertStatement { table_name, rows })
    }

    fn parse_value(&mut self) -> Result<Value, ParseError> {
        let start = self.position;
        match self.current_byte() {
            Some(b'\'') => self.parse_string(),
            Some(byte) if is_identifier_start_byte(byte) => {
                let end = self.scan_identifier();
                let literal = &self.sql[start..end];
                if literal.eq_ignore_ascii_case("true") {
                    Ok(Value::Bool(true))
                } else if literal.eq_ignore_ascii_case("false") {
                    Ok(Value::Bool(false))
                } else if is_non_finite_name(literal) {
                    Err(ParseError::NonFiniteFloat {
                        position: start,
                        literal: literal.to_owned(),
                    })
                } else {
                    Err(self.syntax_error_at("literal", start, Some(end)))
                }
            }
            Some(b'+' | b'-' | b'.' | b'0'..=b'9') => self.parse_number(),
            _ => Err(self.syntax_error("literal")),
        }
    }

    fn parse_number(&mut self) -> Result<Value, ParseError> {
        let start = self.position;
        while let Some(byte) = self.current_byte() {
            if is_value_delimiter(byte) {
                break;
            }
            self.position += 1;
        }

        let literal = &self.sql[start..self.position];
        let unsigned = literal.strip_prefix(['+', '-']).unwrap_or(literal);
        if is_non_finite_name(unsigned) {
            return Err(ParseError::NonFiniteFloat {
                position: start,
                literal: literal.to_owned(),
            });
        }

        match classify_number(literal) {
            Some(NumberKind::Integer) => {
                literal
                    .parse::<i64>()
                    .map(Value::Int64)
                    .map_err(|_| ParseError::IntegerOverflow {
                        position: start,
                        literal: literal.to_owned(),
                    })
            }
            Some(NumberKind::Float) => match literal.parse::<f64>() {
                Ok(value) if value.is_finite() => Ok(Value::Float64(value)),
                Ok(_) | Err(_) => Err(ParseError::NonFiniteFloat {
                    position: start,
                    literal: literal.to_owned(),
                }),
            },
            None => Err(self.syntax_error_at("literal", start, Some(self.position))),
        }
    }

    fn parse_string(&mut self) -> Result<Value, ParseError> {
        let start = self.position;
        let content_start = start + 1;
        let mut cursor = content_start;
        let mut segment_start = content_start;
        let mut decoded_bytes = 0usize;

        let closing_quote = loop {
            let Some(relative_quote) = self.sql[cursor..].find('\'') else {
                return Err(ParseError::Syntax {
                    position: self.sql.len(),
                    expected: "closing quote",
                    found: None,
                });
            };
            let quote = cursor + relative_quote;
            if self.sql.as_bytes().get(quote + 1) == Some(&b'\'') {
                decoded_bytes = decoded_bytes
                    .checked_add(quote - segment_start)
                    .and_then(|bytes| bytes.checked_add(1))
                    .unwrap_or(usize::MAX);
                cursor = quote + 2;
                segment_start = cursor;
            } else {
                decoded_bytes = decoded_bytes
                    .checked_add(quote - segment_start)
                    .unwrap_or(usize::MAX);
                break quote;
            }
        };

        let attempted_bytes = self
            .string_bytes
            .checked_add(decoded_bytes)
            .unwrap_or(usize::MAX);
        if attempted_bytes > self.limits.max_string_bytes {
            return Err(ParseError::StringByteLimitExceeded {
                position: start,
                max_bytes: self.limits.max_string_bytes,
                attempted_bytes,
            });
        }

        let value = self.sql[content_start..closing_quote].replace("''", "'");
        debug_assert_eq!(value.len(), decoded_bytes);
        self.position = closing_quote + 1;
        self.string_bytes = attempted_bytes;
        Ok(Value::String(value))
    }

    fn expect_keyword(&mut self, expected: &'static str) -> Result<(), ParseError> {
        self.skip_whitespace();
        let start = self.position;
        let Some(byte) = self.current_byte() else {
            return Err(self.syntax_error(expected));
        };
        if !is_identifier_start_byte(byte) {
            return Err(self.syntax_error(expected));
        }

        let end = self.scan_identifier();
        if self.sql[start..end].eq_ignore_ascii_case(expected) {
            Ok(())
        } else {
            Err(self.syntax_error_at(expected, start, Some(end)))
        }
    }

    fn expect_identifier(&mut self, expected: &'static str) -> Result<String, ParseError> {
        self.skip_whitespace();
        let start = self.position;
        match self.current_byte() {
            Some(byte) if is_identifier_start_byte(byte) => {
                let end = self.scan_identifier();
                Ok(self.sql[start..end].to_owned())
            }
            _ => Err(self.syntax_error(expected)),
        }
    }

    fn expect_byte(&mut self, expected_byte: u8, expected: &'static str) -> Result<(), ParseError> {
        if self.current_byte() == Some(expected_byte) {
            self.position += 1;
            Ok(())
        } else {
            Err(self.syntax_error(expected))
        }
    }

    fn finish_statement(&mut self) -> Result<(), ParseError> {
        self.skip_whitespace();
        if self.current_byte() == Some(b';') {
            self.position += 1;
            self.skip_whitespace();
        }
        if self.position != self.sql.len() {
            return Err(ParseError::TrailingInput {
                position: self.position,
            });
        }

        Ok(())
    }

    fn scan_identifier(&mut self) -> usize {
        while self.current_byte().is_some_and(is_identifier_continue_byte) {
            self.position += 1;
        }
        self.position
    }

    fn skip_whitespace(&mut self) {
        while self
            .current_byte()
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            self.position += 1;
        }
    }

    fn current_byte(&self) -> Option<u8> {
        self.sql.as_bytes().get(self.position).copied()
    }

    fn syntax_error(&self, expected: &'static str) -> ParseError {
        self.syntax_error_at(expected, self.position, None)
    }

    fn syntax_error_at(
        &self,
        expected: &'static str,
        position: usize,
        known_end: Option<usize>,
    ) -> ParseError {
        let found = if position == self.sql.len() {
            None
        } else {
            let end = known_end.unwrap_or_else(|| {
                self.sql[position..]
                    .chars()
                    .next()
                    .map_or(position, |character| position + character.len_utf8())
            });
            Some(self.sql[position..end].to_owned())
        };
        ParseError::Syntax {
            position,
            expected,
            found,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NumberKind {
    Integer,
    Float,
}

fn classify_number(literal: &str) -> Option<NumberKind> {
    let bytes = literal.as_bytes();
    let mut position = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let integer_start = position;
    while bytes.get(position).is_some_and(u8::is_ascii_digit) {
        position += 1;
    }
    let mut digit_count = position - integer_start;
    let mut kind = NumberKind::Integer;

    if bytes.get(position) == Some(&b'.') {
        kind = NumberKind::Float;
        position += 1;
        let fraction_start = position;
        while bytes.get(position).is_some_and(u8::is_ascii_digit) {
            position += 1;
        }
        digit_count += position - fraction_start;
    }
    if digit_count == 0 {
        return None;
    }

    if matches!(bytes.get(position), Some(b'e' | b'E')) {
        kind = NumberKind::Float;
        position += 1;
        if matches!(bytes.get(position), Some(b'+' | b'-')) {
            position += 1;
        }
        let exponent_start = position;
        while bytes.get(position).is_some_and(u8::is_ascii_digit) {
            position += 1;
        }
        if position == exponent_start {
            return None;
        }
    }

    (position == bytes.len()).then_some(kind)
}

fn is_non_finite_name(literal: &str) -> bool {
    literal.eq_ignore_ascii_case("nan")
        || literal.eq_ignore_ascii_case("inf")
        || literal.eq_ignore_ascii_case("infinity")
}

const fn is_identifier_start_byte(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

const fn is_identifier_continue_byte(byte: u8) -> bool {
    is_identifier_start_byte(byte) || byte.is_ascii_digit()
}

const fn is_value_delimiter(byte: u8) -> bool {
    byte.is_ascii_whitespace() || matches!(byte, b',' | b')' | b';')
}
