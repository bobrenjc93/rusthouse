//! Bounded parsers for the supported SQL statements.

use std::collections::HashMap;
use std::fmt;

pub use crate::storage::DataType as ColumnType;

/// Maximum accepted SQL input size, measured in UTF-8 bytes.
pub const MAX_INPUT_BYTES: usize = 64 * 1024;

/// Maximum number of lexical tokens, excluding the end-of-input marker.
pub const MAX_TOKENS: usize = 1024;

/// Maximum number of columns in one `CREATE TABLE` statement.
pub const MAX_COLUMNS: usize = 256;

/// The syntax tree for one `CREATE TABLE` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateTable {
    pub name: String,
    pub columns: Vec<ColumnDefinition>,
}

/// The syntax tree for `SELECT * FROM <identifier>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectAll {
    pub table_name: String,
}

/// The syntax tree for one `INSERT INTO <identifier> VALUES (...)` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct InsertInto {
    pub table_name: String,
    pub values: Vec<crate::storage::Value>,
}

/// A named column and its declared type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDefinition {
    pub name: String,
    pub column_type: ColumnType,
}

/// A keyword expected at an error location.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyword {
    Create,
    Table,
    Select,
    From,
    Insert,
    Into,
    Values,
}

/// A parse error with a zero-based UTF-8 byte offset into the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub kind: ParseErrorKind,
    pub position: usize,
}

/// The typed reason that parsing failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseErrorKind {
    InputTooLong { limit: usize, actual: usize },
    TooManyTokens { limit: usize },
    TooManyColumns { limit: usize },
    UnexpectedCharacter { character: char },
    ExpectedKeyword { keyword: Keyword },
    ExpectedAsterisk,
    ExpectedIdentifier,
    ExpectedLeftParenthesis,
    ExpectedColumnType,
    ExpectedValue,
    InvalidIntegerLiteral { found: String },
    InvalidFloatLiteral { found: String },
    NonFiniteFloatLiteral { found: String },
    UnterminatedString,
    UnknownColumnType { found: String },
    ExpectedCommaOrRightParenthesis,
    DuplicateColumn { name: String, first_position: usize },
    TrailingInput,
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ParseErrorKind::InputTooLong { limit, actual } => write!(
                formatter,
                "input is {actual} bytes, exceeding the {limit}-byte limit at byte {}",
                self.position
            ),
            ParseErrorKind::TooManyTokens { limit } => write!(
                formatter,
                "token count exceeds the limit of {limit} at byte {}",
                self.position
            ),
            ParseErrorKind::TooManyColumns { limit } => write!(
                formatter,
                "column count exceeds the limit of {limit} at byte {}",
                self.position
            ),
            ParseErrorKind::UnexpectedCharacter { character } => write!(
                formatter,
                "unexpected character {character:?} at byte {}",
                self.position
            ),
            ParseErrorKind::ExpectedKeyword { keyword } => write!(
                formatter,
                "expected keyword {keyword} at byte {}",
                self.position
            ),
            ParseErrorKind::ExpectedAsterisk => {
                write!(formatter, "expected '*' at byte {}", self.position)
            }
            ParseErrorKind::ExpectedIdentifier => {
                write!(formatter, "expected identifier at byte {}", self.position)
            }
            ParseErrorKind::ExpectedLeftParenthesis => {
                write!(formatter, "expected '(' at byte {}", self.position)
            }
            ParseErrorKind::ExpectedColumnType => {
                write!(formatter, "expected column type at byte {}", self.position)
            }
            ParseErrorKind::ExpectedValue => {
                write!(formatter, "expected value at byte {}", self.position)
            }
            ParseErrorKind::InvalidIntegerLiteral { found } => write!(
                formatter,
                "invalid Int64 literal {found:?} at byte {}",
                self.position
            ),
            ParseErrorKind::InvalidFloatLiteral { found } => write!(
                formatter,
                "invalid Float64 literal {found:?} at byte {}",
                self.position
            ),
            ParseErrorKind::NonFiniteFloatLiteral { found } => write!(
                formatter,
                "Float64 literal {found:?} is not finite at byte {}",
                self.position
            ),
            ParseErrorKind::UnterminatedString => {
                write!(
                    formatter,
                    "unterminated string literal at byte {}",
                    self.position
                )
            }
            ParseErrorKind::UnknownColumnType { found } => write!(
                formatter,
                "unknown column type {found:?} at byte {}",
                self.position
            ),
            ParseErrorKind::ExpectedCommaOrRightParenthesis => {
                write!(formatter, "expected ',' or ')' at byte {}", self.position)
            }
            ParseErrorKind::DuplicateColumn {
                name,
                first_position,
            } => write!(
                formatter,
                "duplicate column {name:?} at byte {}; first defined at byte {first_position}",
                self.position
            ),
            ParseErrorKind::TrailingInput => {
                write!(formatter, "trailing input at byte {}", self.position)
            }
        }
    }
}

impl std::error::Error for ParseError {}

impl fmt::Display for Keyword {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Create => formatter.write_str("CREATE"),
            Self::Table => formatter.write_str("TABLE"),
            Self::Select => formatter.write_str("SELECT"),
            Self::From => formatter.write_str("FROM"),
            Self::Insert => formatter.write_str("INSERT"),
            Self::Into => formatter.write_str("INTO"),
            Self::Values => formatter.write_str("VALUES"),
        }
    }
}

/// Parses exactly one bounded `CREATE TABLE` statement without executing it.
///
/// Keywords and type names are ASCII case-insensitive. Unquoted identifiers use
/// `[A-Za-z_][A-Za-z0-9_]*`. At least one column is required, and duplicate
/// column names are compared case-insensitively. One final semicolon is optional.
///
/// ```
/// use rusthouse::{ColumnType, parse_create_table};
///
/// let statement = parse_create_table(
///     "CREATE TABLE events (id Int64, label String, active Bool)",
/// )?;
/// assert_eq!(statement.name, "events");
/// assert_eq!(statement.columns[0].column_type, ColumnType::Int64);
/// # Ok::<(), rusthouse::ParseError>(())
/// ```
pub fn parse_create_table(input: &str) -> Result<CreateTable, ParseError> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(ParseError {
            kind: ParseErrorKind::InputTooLong {
                limit: MAX_INPUT_BYTES,
                actual: input.len(),
            },
            position: MAX_INPUT_BYTES,
        });
    }

    let tokens = tokenize(input)?;
    Parser::new(tokens).parse_create_table()
}

/// Parses exactly one bounded `SELECT * FROM <identifier>` statement.
///
/// Keywords are ASCII case-insensitive. The table name is an unquoted
/// identifier using `[A-Za-z_][A-Za-z0-9_]*`. One final semicolon is optional.
///
/// ```
/// use rusthouse::parse_select_all;
///
/// let statement = parse_select_all("SELECT * FROM events;")?;
/// assert_eq!(statement.table_name, "events");
/// # Ok::<(), rusthouse::ParseError>(())
/// ```
pub fn parse_select_all(input: &str) -> Result<SelectAll, ParseError> {
    let tokens = tokenize_bounded(input)?;
    Parser::new(tokens).parse_select_all()
}

/// Parses exactly one bounded `INSERT INTO <identifier> VALUES (...)` statement.
///
/// The statement contains one schema-ordered tuple. Integer, finite floating
/// point, Boolean, string, and NULL literals are supported. Strings use single
/// quotes and escape a quote by doubling it. One final semicolon is optional.
///
/// ```
/// use rusthouse::{Value, parse_insert};
///
/// let statement = parse_insert("INSERT INTO events VALUES (7, 'it''s ready', true)")?;
/// assert_eq!(statement.table_name, "events");
/// assert_eq!(statement.values[0], Value::Int64(7));
/// assert_eq!(statement.values[1], Value::from("it's ready"));
/// # Ok::<(), rusthouse::ParseError>(())
/// ```
pub fn parse_insert(input: &str) -> Result<InsertInto, ParseError> {
    let tokens = tokenize_bounded(input)?;
    Parser::new(tokens).parse_insert()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenKind<'a> {
    Word(&'a str),
    Number(&'a str),
    String(&'a str),
    Asterisk,
    LeftParenthesis,
    RightParenthesis,
    Comma,
    Semicolon,
    End,
}

fn tokenize_bounded(input: &str) -> Result<Vec<Token<'_>>, ParseError> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(ParseError {
            kind: ParseErrorKind::InputTooLong {
                limit: MAX_INPUT_BYTES,
                actual: input.len(),
            },
            position: MAX_INPUT_BYTES,
        });
    }

    tokenize(input)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Token<'a> {
    kind: TokenKind<'a>,
    position: usize,
}

fn tokenize(input: &str) -> Result<Vec<Token<'_>>, ParseError> {
    let bytes = input.as_bytes();
    let mut tokens = Vec::new();
    let mut position = 0;

    while position < bytes.len() {
        let byte = bytes[position];
        if byte.is_ascii_whitespace() {
            position += 1;
            continue;
        }

        let token_position = position;
        let kind = if is_number_start(bytes, position) {
            position += 1;
            while position < bytes.len()
                && !bytes[position].is_ascii_whitespace()
                && !matches!(bytes[position], b',' | b'(' | b')' | b';' | b'\'')
            {
                position += 1;
            }
            TokenKind::Number(&input[token_position..position])
        } else if byte.is_ascii_alphanumeric() || byte == b'_' {
            position += 1;
            while position < bytes.len()
                && (bytes[position].is_ascii_alphanumeric() || bytes[position] == b'_')
            {
                position += 1;
            }
            TokenKind::Word(&input[token_position..position])
        } else if byte == b'\'' {
            position += 1;
            let contents_start = position;
            loop {
                if position == bytes.len() {
                    return Err(ParseError {
                        kind: ParseErrorKind::UnterminatedString,
                        position: token_position,
                    });
                }
                if bytes[position] == b'\'' {
                    if bytes.get(position + 1) == Some(&b'\'') {
                        position += 2;
                        continue;
                    }

                    let contents = &input[contents_start..position];
                    position += 1;
                    break TokenKind::String(contents);
                }
                position += 1;
            }
        } else {
            position += 1;
            match byte {
                b'*' => TokenKind::Asterisk,
                b'(' => TokenKind::LeftParenthesis,
                b')' => TokenKind::RightParenthesis,
                b',' => TokenKind::Comma,
                b';' => TokenKind::Semicolon,
                _ => {
                    let character = input[token_position..].chars().next().unwrap();
                    return Err(ParseError {
                        kind: ParseErrorKind::UnexpectedCharacter { character },
                        position: token_position,
                    });
                }
            }
        };

        if tokens.len() == MAX_TOKENS {
            return Err(ParseError {
                kind: ParseErrorKind::TooManyTokens { limit: MAX_TOKENS },
                position: token_position,
            });
        }
        tokens.push(Token {
            kind,
            position: token_position,
        });
    }

    tokens.push(Token {
        kind: TokenKind::End,
        position: input.len(),
    });
    Ok(tokens)
}

fn is_number_start(bytes: &[u8], position: usize) -> bool {
    match bytes[position] {
        byte if byte.is_ascii_digit() => true,
        b'.' => bytes
            .get(position + 1)
            .is_some_and(|next| next.is_ascii_digit()),
        b'+' | b'-' => {
            bytes
                .get(position + 1)
                .is_some_and(|next| next.is_ascii_digit())
                || (bytes.get(position + 1) == Some(&b'.')
                    && bytes
                        .get(position + 2)
                        .is_some_and(|next| next.is_ascii_digit()))
        }
        _ => false,
    }
}

struct Parser<'a> {
    tokens: Vec<Token<'a>>,
    current: usize,
}

impl<'a> Parser<'a> {
    fn new(tokens: Vec<Token<'a>>) -> Self {
        Self { tokens, current: 0 }
    }

    fn parse_create_table(mut self) -> Result<CreateTable, ParseError> {
        self.expect_keyword("CREATE", Keyword::Create)?;
        self.expect_keyword("TABLE", Keyword::Table)?;
        let (table_name, _) = self.parse_identifier()?;
        self.expect_left_parenthesis()?;

        let mut columns = Vec::new();
        let mut defined_columns = HashMap::new();

        loop {
            let (column_name, column_position) = self.parse_identifier()?;
            let column_type = self.parse_column_type()?;

            if columns.len() == MAX_COLUMNS {
                return Err(ParseError {
                    kind: ParseErrorKind::TooManyColumns { limit: MAX_COLUMNS },
                    position: column_position,
                });
            }

            let normalized_name = column_name.to_ascii_lowercase();
            if let Some(first_position) = defined_columns.get(&normalized_name) {
                return Err(ParseError {
                    kind: ParseErrorKind::DuplicateColumn {
                        name: column_name.to_owned(),
                        first_position: *first_position,
                    },
                    position: column_position,
                });
            }
            defined_columns.insert(normalized_name, column_position);
            columns.push(ColumnDefinition {
                name: column_name.to_owned(),
                column_type,
            });

            match self.current_token().kind {
                TokenKind::Comma => self.advance(),
                TokenKind::RightParenthesis => {
                    self.advance();
                    break;
                }
                _ => {
                    return Err(ParseError {
                        kind: ParseErrorKind::ExpectedCommaOrRightParenthesis,
                        position: self.current_token().position,
                    });
                }
            }
        }

        if self.current_token().kind == TokenKind::Semicolon {
            self.advance();
        }
        if self.current_token().kind != TokenKind::End {
            return Err(ParseError {
                kind: ParseErrorKind::TrailingInput,
                position: self.current_token().position,
            });
        }

        Ok(CreateTable {
            name: table_name.to_owned(),
            columns,
        })
    }

    fn parse_select_all(mut self) -> Result<SelectAll, ParseError> {
        self.expect_keyword("SELECT", Keyword::Select)?;
        self.expect_asterisk()?;
        self.expect_keyword("FROM", Keyword::From)?;
        let (table_name, _) = self.parse_identifier()?;
        self.expect_end()?;

        Ok(SelectAll {
            table_name: table_name.to_owned(),
        })
    }

    fn parse_insert(mut self) -> Result<InsertInto, ParseError> {
        self.expect_keyword("INSERT", Keyword::Insert)?;
        self.expect_keyword("INTO", Keyword::Into)?;
        let (table_name, _) = self.parse_identifier()?;
        self.expect_keyword("VALUES", Keyword::Values)?;
        self.expect_left_parenthesis()?;

        let mut values = Vec::new();
        if self.current_token().kind != TokenKind::RightParenthesis {
            loop {
                values.push(self.parse_value()?);
                match self.current_token().kind {
                    TokenKind::Comma => self.advance(),
                    TokenKind::RightParenthesis => break,
                    _ => {
                        return Err(ParseError {
                            kind: ParseErrorKind::ExpectedCommaOrRightParenthesis,
                            position: self.current_token().position,
                        });
                    }
                }
            }
        }
        self.advance();
        self.expect_end()?;

        Ok(InsertInto {
            table_name: table_name.to_owned(),
            values,
        })
    }

    fn expect_keyword(&mut self, expected: &str, keyword: Keyword) -> Result<(), ParseError> {
        if matches!(self.current_token().kind, TokenKind::Word(word) if word.eq_ignore_ascii_case(expected))
        {
            self.advance();
            Ok(())
        } else {
            Err(ParseError {
                kind: ParseErrorKind::ExpectedKeyword { keyword },
                position: self.current_token().position,
            })
        }
    }

    fn parse_identifier(&mut self) -> Result<(&'a str, usize), ParseError> {
        let token = *self.current_token();
        if let TokenKind::Word(word) = token.kind {
            if word
                .as_bytes()
                .first()
                .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
            {
                self.advance();
                return Ok((word, token.position));
            }
        }

        Err(ParseError {
            kind: ParseErrorKind::ExpectedIdentifier,
            position: token.position,
        })
    }

    fn expect_asterisk(&mut self) -> Result<(), ParseError> {
        if self.current_token().kind == TokenKind::Asterisk {
            self.advance();
            Ok(())
        } else {
            Err(ParseError {
                kind: ParseErrorKind::ExpectedAsterisk,
                position: self.current_token().position,
            })
        }
    }

    fn expect_end(&mut self) -> Result<(), ParseError> {
        if self.current_token().kind == TokenKind::Semicolon {
            self.advance();
        }
        if self.current_token().kind != TokenKind::End {
            return Err(ParseError {
                kind: ParseErrorKind::TrailingInput,
                position: self.current_token().position,
            });
        }
        Ok(())
    }

    fn expect_left_parenthesis(&mut self) -> Result<(), ParseError> {
        if self.current_token().kind == TokenKind::LeftParenthesis {
            self.advance();
            Ok(())
        } else {
            Err(ParseError {
                kind: ParseErrorKind::ExpectedLeftParenthesis,
                position: self.current_token().position,
            })
        }
    }

    fn parse_column_type(&mut self) -> Result<ColumnType, ParseError> {
        let token = *self.current_token();
        let TokenKind::Word(word) = token.kind else {
            return Err(ParseError {
                kind: ParseErrorKind::ExpectedColumnType,
                position: token.position,
            });
        };

        let column_type = if word.eq_ignore_ascii_case("Int64") {
            ColumnType::Int64
        } else if word.eq_ignore_ascii_case("Float64") {
            ColumnType::Float64
        } else if word.eq_ignore_ascii_case("Bool") {
            ColumnType::Bool
        } else if word.eq_ignore_ascii_case("String") {
            ColumnType::String
        } else {
            return Err(ParseError {
                kind: ParseErrorKind::UnknownColumnType {
                    found: word.to_owned(),
                },
                position: token.position,
            });
        };

        self.advance();
        Ok(column_type)
    }

    fn parse_value(&mut self) -> Result<crate::storage::Value, ParseError> {
        use crate::storage::Value;

        let token = *self.current_token();
        let value = match token.kind {
            TokenKind::Word(word) if word.eq_ignore_ascii_case("NULL") => Value::Null,
            TokenKind::Word(word) if word.eq_ignore_ascii_case("TRUE") => Value::Bool(true),
            TokenKind::Word(word) if word.eq_ignore_ascii_case("FALSE") => Value::Bool(false),
            TokenKind::String(contents) => Value::String(unescape_string(contents)),
            TokenKind::Number(number)
                if number.contains('.') || number.contains('e') || number.contains('E') =>
            {
                let parsed = number.parse::<f64>().map_err(|_| ParseError {
                    kind: ParseErrorKind::InvalidFloatLiteral {
                        found: number.to_owned(),
                    },
                    position: token.position,
                })?;
                if !parsed.is_finite() {
                    return Err(ParseError {
                        kind: ParseErrorKind::NonFiniteFloatLiteral {
                            found: number.to_owned(),
                        },
                        position: token.position,
                    });
                }
                Value::Float64(parsed)
            }
            TokenKind::Number(number) => {
                Value::Int64(number.parse::<i64>().map_err(|_| ParseError {
                    kind: ParseErrorKind::InvalidIntegerLiteral {
                        found: number.to_owned(),
                    },
                    position: token.position,
                })?)
            }
            _ => {
                return Err(ParseError {
                    kind: ParseErrorKind::ExpectedValue,
                    position: token.position,
                });
            }
        };

        self.advance();
        Ok(value)
    }

    fn current_token(&self) -> &Token<'a> {
        &self.tokens[self.current]
    }

    fn advance(&mut self) {
        self.current += 1;
    }
}

fn unescape_string(contents: &str) -> String {
    let mut value = String::with_capacity(contents.len());
    let mut remaining = contents;
    while let Some(position) = remaining.find("''") {
        value.push_str(&remaining[..position]);
        value.push('\'');
        remaining = &remaining[position + 2..];
    }
    value.push_str(remaining);
    value
}
