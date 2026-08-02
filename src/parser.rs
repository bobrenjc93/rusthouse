//! A bounded parser for the supported `CREATE TABLE` syntax.

use std::collections::HashMap;
use std::fmt;

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

/// A named column and its declared type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDefinition {
    pub name: String,
    pub column_type: ColumnType,
}

/// The column types accepted by the initial RustHouse SQL boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    Int64,
    Float64,
    Bool,
    String,
}

/// A keyword expected at an error location.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyword {
    Create,
    Table,
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
    ExpectedIdentifier,
    ExpectedLeftParenthesis,
    ExpectedColumnType,
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
            ParseErrorKind::ExpectedIdentifier => {
                write!(formatter, "expected identifier at byte {}", self.position)
            }
            ParseErrorKind::ExpectedLeftParenthesis => {
                write!(formatter, "expected '(' at byte {}", self.position)
            }
            ParseErrorKind::ExpectedColumnType => {
                write!(formatter, "expected column type at byte {}", self.position)
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
    Parser::new(tokens).parse()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenKind<'a> {
    Word(&'a str),
    LeftParenthesis,
    RightParenthesis,
    Comma,
    Semicolon,
    End,
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
        let kind = if byte.is_ascii_alphanumeric() || byte == b'_' {
            position += 1;
            while position < bytes.len()
                && (bytes[position].is_ascii_alphanumeric() || bytes[position] == b'_')
            {
                position += 1;
            }
            TokenKind::Word(&input[token_position..position])
        } else {
            position += 1;
            match byte {
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

struct Parser<'a> {
    tokens: Vec<Token<'a>>,
    current: usize,
}

impl<'a> Parser<'a> {
    fn new(tokens: Vec<Token<'a>>) -> Self {
        Self { tokens, current: 0 }
    }

    fn parse(mut self) -> Result<CreateTable, ParseError> {
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
        if let TokenKind::Word(word) = token.kind
            && word
                .as_bytes()
                .first()
                .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
        {
            self.advance();
            return Ok((word, token.position));
        }

        Err(ParseError {
            kind: ParseErrorKind::ExpectedIdentifier,
            position: token.position,
        })
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

    fn current_token(&self) -> &Token<'a> {
        &self.tokens[self.current]
    }

    fn advance(&mut self) {
        self.current += 1;
    }
}
