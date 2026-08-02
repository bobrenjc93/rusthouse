//! A bounded lexer for the SQL surface supported by RustHouse.
//!
//! The lexer intentionally does not classify SQL keywords other than Boolean
//! literals. Parsers can interpret identifier tokens according to their
//! current grammar without coupling keyword growth to this module.

use std::error::Error;
use std::fmt;

/// A half-open byte range in the original SQL input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub const fn len(self) -> usize {
        self.end - self.start
    }

    pub const fn is_empty(self) -> bool {
        self.start == self.end
    }
}

/// A token and its byte location in the original SQL input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

/// The lexical token types accepted by the SQL front end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    Identifier(String),
    Integer(String),
    Float(String),
    String(String),
    Boolean(bool),
    LeftParen,
    RightParen,
    Comma,
    Dot,
    Star,
    Plus,
    Minus,
    Slash,
    Percent,
    Equal,
    NotEqual,
    LessThan,
    LessThanOrEqual,
    GreaterThan,
    GreaterThanOrEqual,
    StatementTerminator,
}

/// Resource limits applied by [`tokenize`] and [`Lexer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LexerLimits {
    pub max_input_bytes: usize,
    pub max_tokens: usize,
    pub max_statements: usize,
}

impl LexerLimits {
    pub const DEFAULT: Self = Self {
        max_input_bytes: 1024 * 1024,
        max_tokens: 100_000,
        max_statements: 1_000,
    };

    pub const fn new(max_input_bytes: usize, max_tokens: usize, max_statements: usize) -> Self {
        Self {
            max_input_bytes,
            max_tokens,
            max_statements,
        }
    }
}

impl Default for LexerLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Why a numeric token could not be formed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidNumberReason {
    MissingExponentDigits,
    IdentifierSuffix,
}

/// A typed lexical failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LexErrorKind {
    InputLimitExceeded { limit: usize, actual: usize },
    TokenLimitExceeded { limit: usize },
    StatementLimitExceeded { limit: usize },
    InvalidNumber { reason: InvalidNumberReason },
    InvalidOperator { character: char },
    UnterminatedString,
    UnexpectedCharacter { character: char },
}

/// A lexical failure and its byte location in the original SQL input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexError {
    pub kind: LexErrorKind,
    pub span: Span,
}

impl LexError {
    const fn new(kind: LexErrorKind, span: Span) -> Self {
        Self { kind, span }
    }
}

impl fmt::Display for LexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            LexErrorKind::InputLimitExceeded { limit, actual } => write!(
                formatter,
                "SQL input is {actual} bytes, exceeding the {limit}-byte limit"
            )?,
            LexErrorKind::TokenLimitExceeded { limit } => {
                write!(formatter, "SQL input exceeds the {limit}-token limit")?
            }
            LexErrorKind::StatementLimitExceeded { limit } => {
                write!(formatter, "SQL input exceeds the {limit}-statement limit")?
            }
            LexErrorKind::InvalidNumber { reason } => match reason {
                InvalidNumberReason::MissingExponentDigits => {
                    formatter.write_str("numeric literal exponent has no digits")?
                }
                InvalidNumberReason::IdentifierSuffix => {
                    formatter.write_str("numeric literal has an identifier suffix")?
                }
            },
            LexErrorKind::InvalidOperator { character } => write!(
                formatter,
                "invalid comparison operator starting with {character:?}"
            )?,
            LexErrorKind::UnterminatedString => {
                formatter.write_str("unterminated string literal")?
            }
            LexErrorKind::UnexpectedCharacter { character } => {
                write!(formatter, "unexpected character {character:?}")?
            }
        }

        write!(
            formatter,
            " at bytes {}..{}",
            self.span.start, self.span.end
        )
    }
}

impl Error for LexError {}

/// Tokenizes SQL while enforcing all supplied resource limits.
pub fn tokenize(input: &str, limits: LexerLimits) -> Result<Vec<Token>, LexError> {
    Lexer::new(input, limits).tokenize()
}

/// Stateful implementation of the bounded SQL lexer.
pub struct Lexer<'a> {
    input: &'a str,
    limits: LexerLimits,
    offset: usize,
    statement_count: usize,
    in_statement: bool,
}

impl<'a> Lexer<'a> {
    pub fn new(input: &'a str, limits: LexerLimits) -> Self {
        Self {
            input,
            limits,
            offset: 0,
            statement_count: 0,
            in_statement: false,
        }
    }

    /// Consumes the lexer and returns the complete bounded token stream.
    ///
    /// A lexical statement is a non-empty run of tokens separated by one or
    /// more statement terminators. Empty runs between semicolons do not count.
    pub fn tokenize(mut self) -> Result<Vec<Token>, LexError> {
        self.check_input_limit()?;

        let mut tokens = Vec::new();
        loop {
            self.skip_whitespace();
            if self.offset == self.input.len() {
                return Ok(tokens);
            }

            if tokens.len() == self.limits.max_tokens {
                return Err(LexError::new(
                    LexErrorKind::TokenLimitExceeded {
                        limit: self.limits.max_tokens,
                    },
                    self.current_character_span(),
                ));
            }

            if !self.in_statement
                && !self.remaining().starts_with(';')
                && self.statement_count == self.limits.max_statements
            {
                return Err(LexError::new(
                    LexErrorKind::StatementLimitExceeded {
                        limit: self.limits.max_statements,
                    },
                    self.current_character_span(),
                ));
            }

            let token = self.scan_token()?;
            if token.kind == TokenKind::StatementTerminator {
                self.in_statement = false;
            } else if !self.in_statement {
                self.statement_count += 1;
                self.in_statement = true;
            }
            tokens.push(token);
        }
    }

    fn check_input_limit(&self) -> Result<(), LexError> {
        let actual = self.input.len();
        if actual <= self.limits.max_input_bytes {
            return Ok(());
        }

        let mut start = self.limits.max_input_bytes;
        while !self.input.is_char_boundary(start) {
            start -= 1;
        }
        Err(LexError::new(
            LexErrorKind::InputLimitExceeded {
                limit: self.limits.max_input_bytes,
                actual,
            },
            Span::new(start, actual),
        ))
    }

    fn skip_whitespace(&mut self) {
        while let Some(character) = self.remaining().chars().next() {
            if !character.is_whitespace() {
                break;
            }
            self.offset += character.len_utf8();
        }
    }

    fn scan_token(&mut self) -> Result<Token, LexError> {
        let start = self.offset;
        let character = self.current_character();
        let kind = match character {
            '(' => self.single_character(TokenKind::LeftParen),
            ')' => self.single_character(TokenKind::RightParen),
            ',' => self.single_character(TokenKind::Comma),
            '*' => self.single_character(TokenKind::Star),
            '+' => self.single_character(TokenKind::Plus),
            '-' => self.single_character(TokenKind::Minus),
            '/' => self.single_character(TokenKind::Slash),
            '%' => self.single_character(TokenKind::Percent),
            ';' => self.single_character(TokenKind::StatementTerminator),
            '=' => self.single_character(TokenKind::Equal),
            '!' => {
                if self.remaining().starts_with("!=") {
                    self.offset += 2;
                    TokenKind::NotEqual
                } else {
                    return Err(LexError::new(
                        LexErrorKind::InvalidOperator { character },
                        self.current_character_span(),
                    ));
                }
            }
            '<' => {
                self.offset += 1;
                if self.remaining().starts_with('=') {
                    self.offset += 1;
                    TokenKind::LessThanOrEqual
                } else if self.remaining().starts_with('>') {
                    self.offset += 1;
                    TokenKind::NotEqual
                } else {
                    TokenKind::LessThan
                }
            }
            '>' => {
                self.offset += 1;
                if self.remaining().starts_with('=') {
                    self.offset += 1;
                    TokenKind::GreaterThanOrEqual
                } else {
                    TokenKind::GreaterThan
                }
            }
            '\'' => return self.scan_string(),
            '.' if self.next_byte().is_some_and(|byte| byte.is_ascii_digit()) => {
                return self.scan_number();
            }
            '.' => self.single_character(TokenKind::Dot),
            character if character.is_ascii_digit() => return self.scan_number(),
            character if is_identifier_start(character) => return Ok(self.scan_identifier()),
            character => {
                return Err(LexError::new(
                    LexErrorKind::UnexpectedCharacter { character },
                    self.current_character_span(),
                ));
            }
        };

        Ok(Token {
            kind,
            span: Span::new(start, self.offset),
        })
    }

    fn single_character(&mut self, kind: TokenKind) -> TokenKind {
        self.offset += 1;
        kind
    }

    fn scan_identifier(&mut self) -> Token {
        let start = self.offset;
        self.offset += self.current_character().len_utf8();
        self.consume_identifier_characters();

        let identifier = &self.input[start..self.offset];
        let kind = if identifier.eq_ignore_ascii_case("true") {
            TokenKind::Boolean(true)
        } else if identifier.eq_ignore_ascii_case("false") {
            TokenKind::Boolean(false)
        } else {
            TokenKind::Identifier(identifier.to_owned())
        };
        Token {
            kind,
            span: Span::new(start, self.offset),
        }
    }

    fn scan_number(&mut self) -> Result<Token, LexError> {
        let start = self.offset;
        let mut is_float = false;

        if self.remaining().starts_with('.') {
            is_float = true;
            self.offset += 1;
            self.consume_ascii_digits();
        } else {
            self.consume_ascii_digits();
            if self.remaining().starts_with('.') {
                is_float = true;
                self.offset += 1;
                self.consume_ascii_digits();
            }
        }

        if self.remaining().starts_with(['e', 'E']) {
            is_float = true;
            self.offset += 1;
            if self.remaining().starts_with(['+', '-']) {
                self.offset += 1;
            }
            let exponent_start = self.offset;
            self.consume_ascii_digits();
            if self.offset == exponent_start {
                self.consume_identifier_characters();
                return Err(LexError::new(
                    LexErrorKind::InvalidNumber {
                        reason: InvalidNumberReason::MissingExponentDigits,
                    },
                    Span::new(start, self.offset),
                ));
            }
        }

        if self
            .remaining()
            .chars()
            .next()
            .is_some_and(is_identifier_continue)
        {
            self.consume_identifier_characters();
            return Err(LexError::new(
                LexErrorKind::InvalidNumber {
                    reason: InvalidNumberReason::IdentifierSuffix,
                },
                Span::new(start, self.offset),
            ));
        }

        let literal = self.input[start..self.offset].to_owned();
        Ok(Token {
            kind: if is_float {
                TokenKind::Float(literal)
            } else {
                TokenKind::Integer(literal)
            },
            span: Span::new(start, self.offset),
        })
    }

    fn scan_string(&mut self) -> Result<Token, LexError> {
        let start = self.offset;
        self.offset += 1;
        let mut value = String::new();

        while self.offset < self.input.len() {
            let character = self.current_character();
            if character == '\'' {
                self.offset += 1;
                if self.remaining().starts_with('\'') {
                    value.push('\'');
                    self.offset += 1;
                } else {
                    return Ok(Token {
                        kind: TokenKind::String(value),
                        span: Span::new(start, self.offset),
                    });
                }
            } else {
                value.push(character);
                self.offset += character.len_utf8();
            }
        }

        Err(LexError::new(
            LexErrorKind::UnterminatedString,
            Span::new(start, self.offset),
        ))
    }

    fn consume_ascii_digits(&mut self) {
        while self
            .input
            .as_bytes()
            .get(self.offset)
            .is_some_and(u8::is_ascii_digit)
        {
            self.offset += 1;
        }
    }

    fn consume_identifier_characters(&mut self) {
        while let Some(character) = self.remaining().chars().next() {
            if !is_identifier_continue(character) {
                break;
            }
            self.offset += character.len_utf8();
        }
    }

    fn remaining(&self) -> &'a str {
        &self.input[self.offset..]
    }

    fn current_character(&self) -> char {
        self.remaining()
            .chars()
            .next()
            .expect("current character is only read before end of input")
    }

    fn current_character_span(&self) -> Span {
        Span::new(
            self.offset,
            self.offset + self.current_character().len_utf8(),
        )
    }

    fn next_byte(&self) -> Option<u8> {
        self.input.as_bytes().get(self.offset + 1).copied()
    }
}

fn is_identifier_start(character: char) -> bool {
    character == '_' || character.is_alphabetic()
}

fn is_identifier_continue(character: char) -> bool {
    character == '_' || character.is_alphanumeric()
}
