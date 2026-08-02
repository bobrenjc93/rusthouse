//! Parsing for RustHouse's bounded SQL surface.

use std::error::Error;
use std::fmt;

/// Maximum SQL statement size accepted by [`parse_create_table`], in bytes.
pub const MAX_SQL_BYTES: usize = 64 * 1024;

/// Maximum number of columns accepted by [`parse_create_table`].
pub const MAX_COLUMNS: usize = 1_024;

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

/// A column type supported by the first RustHouse SQL grammar.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataType {
    Int64,
    Float64,
    Bool,
    String,
}

impl fmt::Display for DataType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Int64 => formatter.write_str("Int64"),
            Self::Float64 => formatter.write_str("Float64"),
            Self::Bool => formatter.write_str("Bool"),
            Self::String => formatter.write_str("String"),
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
}

impl ParseError {
    /// Returns the zero-based byte offset associated with this error.
    pub const fn position(&self) -> usize {
        match self {
            Self::Syntax { position, .. }
            | Self::UnsupportedType { position, .. }
            | Self::TrailingInput { position }
            | Self::SqlTooLarge { position, .. }
            | Self::TooManyColumns { position, .. } => *position,
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
    if sql.len() > limits.max_sql_bytes {
        return Err(ParseError::SqlTooLarge {
            position: limits.max_sql_bytes,
            max_bytes: limits.max_sql_bytes,
            actual_bytes: sql.len(),
        });
    }

    Parser::new(sql, limits.max_columns).parse()
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
