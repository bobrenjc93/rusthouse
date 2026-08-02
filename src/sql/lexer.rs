//! A bounded lexer for the SQL surface supported by RustHouse.
//!
//! The lexer deliberately treats SQL keywords as identifiers. Deciding whether
//! an identifier is a keyword depends on the grammar position and belongs in
//! the parser. Boolean literals are tokens in their own right.

use std::error::Error;
use std::fmt;

/// Maximum input size used by [`tokenize`].
pub const DEFAULT_MAX_INPUT_BYTES: usize = 1024 * 1024;

/// Maximum number of tokens produced by [`tokenize`].
pub const DEFAULT_MAX_TOKENS: usize = 100_000;

/// Resource limits applied while lexing SQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LexerLimits {
    /// Maximum accepted SQL input size in bytes.
    pub max_input_bytes: usize,
    /// Maximum number of tokens that may be emitted.
    pub max_tokens: usize,
}

impl Default for LexerLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }
}

/// An exclusive byte range in the original SQL input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// Byte offset of the token's first character.
    pub start: usize,
    /// Byte offset immediately after the token.
    pub end: usize,
}

/// A lexical SQL token and its location in the original input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// The token's semantic value.
    pub kind: TokenKind,
    /// The token's location in the original SQL input.
    pub span: Span,
}

/// The token types recognized by the SQL lexer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    /// An unquoted identifier, preserving its original case.
    Identifier(String),
    /// A numeric literal preserving its original spelling.
    Number(String),
    /// A case-insensitive `TRUE` or `FALSE` literal.
    Boolean(bool),
    /// A string literal with doubled single quotes decoded.
    String(String),
    /// An arithmetic or comparison operator.
    Operator(Operator),
    /// An expression or statement delimiter.
    Punctuation(Punctuation),
    /// A semicolon separating or terminating SQL statements.
    Terminator,
}

/// Operators used by arithmetic and comparison expressions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operator {
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    Plus,
    Minus,
    Multiply,
    Divide,
    Modulo,
}

/// Delimiters used by SQL statements and expressions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Punctuation {
    Comma,
    LeftParen,
    RightParen,
    Dot,
}

/// A lexical error with a byte offset into the original SQL input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LexError {
    InputTooLong {
        offset: usize,
        actual_bytes: usize,
        max_bytes: usize,
    },
    TooManyTokens {
        offset: usize,
        max_tokens: usize,
    },
    UnterminatedString {
        offset: usize,
    },
    InvalidNumber {
        offset: usize,
    },
    UnexpectedCharacter {
        offset: usize,
        character: char,
    },
}

impl LexError {
    /// Returns the byte offset at which lexing failed.
    pub fn offset(&self) -> usize {
        match *self {
            Self::InputTooLong { offset, .. }
            | Self::TooManyTokens { offset, .. }
            | Self::UnterminatedString { offset }
            | Self::InvalidNumber { offset }
            | Self::UnexpectedCharacter { offset, .. } => offset,
        }
    }
}

impl fmt::Display for LexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputTooLong {
                offset,
                actual_bytes,
                max_bytes,
            } => write!(
                formatter,
                "SQL input has {actual_bytes} bytes, exceeding the {max_bytes}-byte limit at byte {offset}"
            ),
            Self::TooManyTokens { offset, max_tokens } => write!(
                formatter,
                "SQL token count exceeds the {max_tokens}-token limit at byte {offset}"
            ),
            Self::UnterminatedString { offset } => {
                write!(formatter, "unterminated string starting at byte {offset}")
            }
            Self::InvalidNumber { offset } => {
                write!(formatter, "invalid number at byte {offset}")
            }
            Self::UnexpectedCharacter { offset, character } => write!(
                formatter,
                "unexpected character {character:?} at byte {offset}"
            ),
        }
    }
}

impl Error for LexError {}

/// Tokenizes SQL using [`LexerLimits::default`].
pub fn tokenize(input: &str) -> Result<Vec<Token>, LexError> {
    tokenize_with_limits(input, LexerLimits::default())
}

/// Tokenizes SQL while enforcing the supplied input and token limits.
pub fn tokenize_with_limits(input: &str, limits: LexerLimits) -> Result<Vec<Token>, LexError> {
    if input.len() > limits.max_input_bytes {
        return Err(LexError::InputTooLong {
            offset: limits.max_input_bytes,
            actual_bytes: input.len(),
            max_bytes: limits.max_input_bytes,
        });
    }

    Lexer {
        input,
        offset: 0,
        limits,
        tokens: Vec::new(),
    }
    .tokenize()
}

struct Lexer<'a> {
    input: &'a str,
    offset: usize,
    limits: LexerLimits,
    tokens: Vec<Token>,
}

impl Lexer<'_> {
    fn tokenize(mut self) -> Result<Vec<Token>, LexError> {
        while self.offset < self.input.len() {
            let byte = self.bytes()[self.offset];

            if byte.is_ascii_whitespace() {
                self.offset += 1;
                continue;
            }

            let start = self.offset;
            if self.tokens.len() >= self.limits.max_tokens {
                return Err(LexError::TooManyTokens {
                    offset: start,
                    max_tokens: self.limits.max_tokens,
                });
            }

            let kind = match byte {
                b'a'..=b'z' | b'A'..=b'Z' | b'_' => self.identifier(),
                b'0'..=b'9' => self.number(false)?,
                b'.' if self.peek(1).is_some_and(|next| next.is_ascii_digit()) => {
                    self.number(true)?
                }
                b'\'' => self.string(start)?,
                b'=' => {
                    self.offset += 1;
                    TokenKind::Operator(Operator::Equal)
                }
                b'!' if self.peek(1) == Some(b'=') => {
                    self.offset += 2;
                    TokenKind::Operator(Operator::NotEqual)
                }
                b'<' => {
                    self.offset += 1;
                    let operator = match self.current() {
                        Some(b'=') => {
                            self.offset += 1;
                            Operator::LessEqual
                        }
                        Some(b'>') => {
                            self.offset += 1;
                            Operator::NotEqual
                        }
                        _ => Operator::Less,
                    };
                    TokenKind::Operator(operator)
                }
                b'>' => {
                    self.offset += 1;
                    let operator = if self.current() == Some(b'=') {
                        self.offset += 1;
                        Operator::GreaterEqual
                    } else {
                        Operator::Greater
                    };
                    TokenKind::Operator(operator)
                }
                b'+' => self.single_byte_operator(Operator::Plus),
                b'-' => self.single_byte_operator(Operator::Minus),
                b'*' => self.single_byte_operator(Operator::Multiply),
                b'/' => self.single_byte_operator(Operator::Divide),
                b'%' => self.single_byte_operator(Operator::Modulo),
                b',' => self.single_byte_punctuation(Punctuation::Comma),
                b'(' => self.single_byte_punctuation(Punctuation::LeftParen),
                b')' => self.single_byte_punctuation(Punctuation::RightParen),
                b'.' => self.single_byte_punctuation(Punctuation::Dot),
                b';' => {
                    self.offset += 1;
                    TokenKind::Terminator
                }
                _ => {
                    let character = self.input[start..]
                        .chars()
                        .next()
                        .expect("offset is inside the input");
                    return Err(LexError::UnexpectedCharacter {
                        offset: start,
                        character,
                    });
                }
            };

            self.tokens.push(Token {
                kind,
                span: Span {
                    start,
                    end: self.offset,
                },
            });
        }

        Ok(self.tokens)
    }

    fn identifier(&mut self) -> TokenKind {
        let start = self.offset;
        self.offset += 1;
        while self.current().is_some_and(is_identifier_continue) {
            self.offset += 1;
        }

        let identifier = &self.input[start..self.offset];
        if identifier.eq_ignore_ascii_case("true") {
            TokenKind::Boolean(true)
        } else if identifier.eq_ignore_ascii_case("false") {
            TokenKind::Boolean(false)
        } else {
            TokenKind::Identifier(identifier.to_owned())
        }
    }

    fn number(&mut self, starts_with_dot: bool) -> Result<TokenKind, LexError> {
        let start = self.offset;

        if starts_with_dot {
            self.offset += 1;
        }
        self.consume_ascii_digits();

        if !starts_with_dot && self.current() == Some(b'.') {
            self.offset += 1;
            self.consume_ascii_digits();
        }

        if matches!(self.current(), Some(b'e' | b'E')) {
            let exponent_offset = self.offset;
            self.offset += 1;
            if matches!(self.current(), Some(b'+' | b'-')) {
                self.offset += 1;
            }
            if !self.current().is_some_and(|byte| byte.is_ascii_digit()) {
                return Err(LexError::InvalidNumber {
                    offset: exponent_offset,
                });
            }
            self.consume_ascii_digits();
        }

        Ok(TokenKind::Number(self.input[start..self.offset].to_owned()))
    }

    fn string(&mut self, start: usize) -> Result<TokenKind, LexError> {
        self.offset += 1;
        let mut value = String::new();
        let mut segment_start = self.offset;

        while self.offset < self.input.len() {
            if self.bytes()[self.offset] != b'\'' {
                self.offset += 1;
                continue;
            }

            value.push_str(&self.input[segment_start..self.offset]);
            if self.peek(1) == Some(b'\'') {
                value.push('\'');
                self.offset += 2;
                segment_start = self.offset;
                continue;
            }

            self.offset += 1;
            return Ok(TokenKind::String(value));
        }

        Err(LexError::UnterminatedString { offset: start })
    }

    fn consume_ascii_digits(&mut self) {
        while self.current().is_some_and(|byte| byte.is_ascii_digit()) {
            self.offset += 1;
        }
    }

    fn single_byte_operator(&mut self, operator: Operator) -> TokenKind {
        self.offset += 1;
        TokenKind::Operator(operator)
    }

    fn single_byte_punctuation(&mut self, punctuation: Punctuation) -> TokenKind {
        self.offset += 1;
        TokenKind::Punctuation(punctuation)
    }

    fn bytes(&self) -> &[u8] {
        self.input.as_bytes()
    }

    fn current(&self) -> Option<u8> {
        self.bytes().get(self.offset).copied()
    }

    fn peek(&self, distance: usize) -> Option<u8> {
        self.offset
            .checked_add(distance)
            .and_then(|offset| self.bytes().get(offset))
            .copied()
    }
}

fn is_identifier_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}
