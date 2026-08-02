//! A bounded lexer for RustHouse's SQL surface.
//!
//! The lexer preserves source locations for every token. Positions use byte
//! offsets and one-based line and column numbers; spans are half-open.

use std::fmt;
use std::ops::Range;

/// Default maximum size of a SQL batch, in bytes.
pub const DEFAULT_MAX_INPUT_BYTES: usize = 16 * 1024 * 1024;

/// Default maximum number of tokens in a SQL batch.
pub const DEFAULT_MAX_TOKENS: usize = 1_000_000;

/// Resource limits applied while lexing a SQL batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LexerConfig {
    pub max_input_bytes: usize,
    pub max_tokens: usize,
}

impl Default for LexerConfig {
    fn default() -> Self {
        Self {
            max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }
}

/// A location in the original SQL text.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Position {
    /// Zero-based byte offset.
    pub byte_offset: usize,
    /// One-based line number.
    pub line: usize,
    /// One-based column number, counted in Unicode scalar values.
    pub column: usize,
}

impl Position {
    const START: Self = Self {
        byte_offset: 0,
        line: 1,
        column: 1,
    };
}

impl fmt::Display for Position {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "line {}, column {} (byte {})",
            self.line, self.column, self.byte_offset
        )
    }
}

/// A half-open range in the original SQL text.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Span {
    pub start: Position,
    pub end: Position,
}

impl Span {
    /// Returns the byte range covered by this span.
    pub fn byte_range(self) -> Range<usize> {
        self.start.byte_offset..self.end.byte_offset
    }
}

/// Operators recognized by the lexer.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Operator {
    Equal,
    NotEqual,
    LessThan,
    LessThanOrEqual,
    GreaterThan,
    GreaterThanOrEqual,
    Plus,
    Minus,
    Multiply,
    Divide,
    Modulo,
    Concatenate,
}

/// Delimiters and separators recognized by the lexer.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Punctuation {
    LeftParenthesis,
    RightParenthesis,
    LeftBracket,
    RightBracket,
    Comma,
    Dot,
}

/// The semantic value of a SQL token.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum TokenKind {
    Identifier(String),
    Number(String),
    /// A single-quoted string with doubled single quotes already decoded.
    String(String),
    Operator(Operator),
    Punctuation(Punctuation),
    Semicolon,
}

/// A token and its location in the original SQL text.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SpannedToken {
    pub kind: TokenKind,
    pub span: Span,
}

impl SpannedToken {
    /// Returns the original, undecoded source text for this token.
    pub fn source<'a>(&self, input: &'a str) -> Option<&'a str> {
        input.get(self.span.byte_range())
    }
}

/// A typed lexing failure with its source position.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LexError {
    InputTooLarge {
        limit: usize,
        actual: usize,
        position: Position,
    },
    TokenLimitExceeded {
        limit: usize,
        position: Position,
    },
    UnexpectedCharacter {
        character: char,
        position: Position,
    },
    UnterminatedString {
        position: Position,
    },
    InvalidNumber {
        position: Position,
    },
    UnterminatedBlockComment {
        position: Position,
    },
}

impl LexError {
    /// Returns the position at which this error was detected or introduced.
    pub fn position(&self) -> Position {
        match *self {
            Self::InputTooLarge { position, .. }
            | Self::TokenLimitExceeded { position, .. }
            | Self::UnexpectedCharacter { position, .. }
            | Self::UnterminatedString { position }
            | Self::InvalidNumber { position }
            | Self::UnterminatedBlockComment { position } => position,
        }
    }
}

impl fmt::Display for LexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputTooLarge {
                limit,
                actual,
                position,
            } => write!(
                formatter,
                "SQL input is {actual} bytes, exceeding the {limit}-byte limit at {position}"
            ),
            Self::TokenLimitExceeded { limit, position } => {
                write!(
                    formatter,
                    "SQL token limit of {limit} exceeded at {position}"
                )
            }
            Self::UnexpectedCharacter {
                character,
                position,
            } => write!(
                formatter,
                "unexpected character {character:?} at {position}"
            ),
            Self::UnterminatedString { position } => {
                write!(formatter, "unterminated string starting at {position}")
            }
            Self::InvalidNumber { position } => {
                write!(formatter, "invalid number exponent at {position}")
            }
            Self::UnterminatedBlockComment { position } => {
                write!(
                    formatter,
                    "unterminated block comment starting at {position}"
                )
            }
        }
    }
}

impl std::error::Error for LexError {}

/// Tokenizes SQL with the default resource limits.
pub fn tokenize(input: &str) -> Result<Vec<SpannedToken>, LexError> {
    tokenize_with_config(input, LexerConfig::default())
}

/// Tokenizes SQL with explicit byte and token limits.
pub fn tokenize_with_config(
    input: &str,
    config: LexerConfig,
) -> Result<Vec<SpannedToken>, LexError> {
    Lexer::new(input, config)?.tokenize()
}

/// Stateful SQL lexer. Most callers should use [`tokenize`] or
/// [`tokenize_with_config`].
#[derive(Debug)]
pub struct Lexer<'a> {
    input: &'a str,
    config: LexerConfig,
    position: Position,
}

impl<'a> Lexer<'a> {
    /// Creates a lexer after checking the configured byte limit.
    pub fn new(input: &'a str, config: LexerConfig) -> Result<Self, LexError> {
        if input.len() > config.max_input_bytes {
            return Err(LexError::InputTooLarge {
                limit: config.max_input_bytes,
                actual: input.len(),
                position: position_at_byte(input, config.max_input_bytes),
            });
        }

        Ok(Self {
            input,
            config,
            position: Position::START,
        })
    }

    /// Consumes the lexer and returns all non-trivia tokens.
    pub fn tokenize(mut self) -> Result<Vec<SpannedToken>, LexError> {
        let initial_capacity = (self.input.len() / 4).min(self.config.max_tokens).min(4096);
        let mut tokens = Vec::with_capacity(initial_capacity);

        loop {
            self.skip_trivia()?;
            if self.peek().is_none() {
                return Ok(tokens);
            }
            if tokens.len() == self.config.max_tokens {
                return Err(LexError::TokenLimitExceeded {
                    limit: self.config.max_tokens,
                    position: self.position,
                });
            }

            tokens.push(self.next_token()?);
        }
    }

    fn next_token(&mut self) -> Result<SpannedToken, LexError> {
        let start = self.position;
        let character = self.peek().expect("next_token is only called before EOF");

        let kind = if is_identifier_start(character) {
            self.identifier()
        } else if character.is_ascii_digit()
            || (character == '.' && self.peek_second().is_some_and(|next| next.is_ascii_digit()))
        {
            self.number()?
        } else {
            match character {
                '\'' => self.string(start)?,
                ';' => {
                    self.advance();
                    TokenKind::Semicolon
                }
                '(' => self.punctuation(Punctuation::LeftParenthesis),
                ')' => self.punctuation(Punctuation::RightParenthesis),
                '[' => self.punctuation(Punctuation::LeftBracket),
                ']' => self.punctuation(Punctuation::RightBracket),
                ',' => self.punctuation(Punctuation::Comma),
                '.' => self.punctuation(Punctuation::Dot),
                '=' => {
                    self.advance();
                    if self.peek() == Some('=') {
                        self.advance();
                    }
                    TokenKind::Operator(Operator::Equal)
                }
                '!' if self.peek_second() == Some('=') => {
                    self.advance();
                    self.advance();
                    TokenKind::Operator(Operator::NotEqual)
                }
                '<' => {
                    self.advance();
                    match self.peek() {
                        Some('=') => {
                            self.advance();
                            TokenKind::Operator(Operator::LessThanOrEqual)
                        }
                        Some('>') => {
                            self.advance();
                            TokenKind::Operator(Operator::NotEqual)
                        }
                        _ => TokenKind::Operator(Operator::LessThan),
                    }
                }
                '>' => {
                    self.advance();
                    if self.peek() == Some('=') {
                        self.advance();
                        TokenKind::Operator(Operator::GreaterThanOrEqual)
                    } else {
                        TokenKind::Operator(Operator::GreaterThan)
                    }
                }
                '+' => self.operator(Operator::Plus),
                '-' => self.operator(Operator::Minus),
                '*' => self.operator(Operator::Multiply),
                '/' => self.operator(Operator::Divide),
                '%' => self.operator(Operator::Modulo),
                '|' if self.peek_second() == Some('|') => {
                    self.advance();
                    self.advance();
                    TokenKind::Operator(Operator::Concatenate)
                }
                unexpected => {
                    return Err(LexError::UnexpectedCharacter {
                        character: unexpected,
                        position: start,
                    });
                }
            }
        };

        Ok(SpannedToken {
            kind,
            span: Span {
                start,
                end: self.position,
            },
        })
    }

    fn identifier(&mut self) -> TokenKind {
        let start = self.position.byte_offset;
        self.advance();
        while self.peek().is_some_and(is_identifier_continue) {
            self.advance();
        }
        TokenKind::Identifier(self.input[start..self.position.byte_offset].to_owned())
    }

    fn number(&mut self) -> Result<TokenKind, LexError> {
        let start = self.position.byte_offset;

        if self.peek() == Some('.') {
            self.advance();
            self.consume_ascii_digits();
        } else {
            self.consume_ascii_digits();
            if self.peek() == Some('.') {
                self.advance();
                self.consume_ascii_digits();
            }
        }

        if matches!(self.peek(), Some('e' | 'E')) {
            let exponent_position = self.position;
            self.advance();
            if matches!(self.peek(), Some('+' | '-')) {
                self.advance();
            }
            if !self
                .peek()
                .is_some_and(|character| character.is_ascii_digit())
            {
                return Err(LexError::InvalidNumber {
                    position: exponent_position,
                });
            }
            self.consume_ascii_digits();
        }

        Ok(TokenKind::Number(
            self.input[start..self.position.byte_offset].to_owned(),
        ))
    }

    fn consume_ascii_digits(&mut self) {
        while self
            .peek()
            .is_some_and(|character| character.is_ascii_digit())
        {
            self.advance();
        }
    }

    fn string(&mut self, start: Position) -> Result<TokenKind, LexError> {
        self.advance();
        let mut decoded = String::new();

        while let Some(character) = self.peek() {
            if character == '\'' {
                self.advance();
                if self.peek() == Some('\'') {
                    self.advance();
                    decoded.push('\'');
                } else {
                    return Ok(TokenKind::String(decoded));
                }
            } else {
                self.advance();
                decoded.push(character);
            }
        }

        Err(LexError::UnterminatedString { position: start })
    }

    fn punctuation(&mut self, punctuation: Punctuation) -> TokenKind {
        self.advance();
        TokenKind::Punctuation(punctuation)
    }

    fn operator(&mut self, operator: Operator) -> TokenKind {
        self.advance();
        TokenKind::Operator(operator)
    }

    fn skip_trivia(&mut self) -> Result<(), LexError> {
        loop {
            while self.peek().is_some_and(char::is_whitespace) {
                self.advance();
            }

            if self.peek() == Some('-') && self.peek_second() == Some('-') {
                self.advance();
                self.advance();
                while self
                    .peek()
                    .is_some_and(|character| !matches!(character, '\n' | '\r'))
                {
                    self.advance();
                }
            } else if self.peek() == Some('/') && self.peek_second() == Some('*') {
                let start = self.position;
                self.advance();
                self.advance();
                loop {
                    match (self.peek(), self.peek_second()) {
                        (Some('*'), Some('/')) => {
                            self.advance();
                            self.advance();
                            break;
                        }
                        (Some(_), _) => {
                            self.advance();
                        }
                        (None, _) => {
                            return Err(LexError::UnterminatedBlockComment { position: start });
                        }
                    }
                }
            } else {
                return Ok(());
            }
        }
    }

    fn peek(&self) -> Option<char> {
        self.input[self.position.byte_offset..].chars().next()
    }

    fn peek_second(&self) -> Option<char> {
        self.input[self.position.byte_offset..].chars().nth(1)
    }

    fn advance(&mut self) -> Option<char> {
        let character = self.peek()?;
        let follows_carriage_return = character == '\n'
            && self.position.byte_offset > 0
            && self.input.as_bytes()[self.position.byte_offset - 1] == b'\r';
        self.position.byte_offset += character.len_utf8();
        if character == '\r' || (character == '\n' && !follows_carriage_return) {
            self.position.line += 1;
            self.position.column = 1;
        } else if character == '\n' {
            self.position.column = 1;
        } else {
            self.position.column += 1;
        }
        Some(character)
    }
}

fn is_identifier_start(character: char) -> bool {
    character == '_' || character.is_alphabetic()
}

fn is_identifier_continue(character: char) -> bool {
    character == '_' || character.is_alphanumeric()
}

fn position_at_byte(input: &str, byte_offset: usize) -> Position {
    let mut position = Position::START;
    let mut previous_was_carriage_return = false;
    for (offset, character) in input.char_indices() {
        if offset + character.len_utf8() > byte_offset {
            break;
        }
        position.byte_offset = offset + character.len_utf8();
        if character == '\r' || (character == '\n' && !previous_was_carriage_return) {
            position.line += 1;
            position.column = 1;
        } else if character == '\n' {
            position.column = 1;
        } else {
            position.column += 1;
        }
        previous_was_carriage_return = character == '\r';
    }
    position.byte_offset = byte_offset;
    position
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(input: &str) -> Vec<TokenKind> {
        tokenize(input)
            .expect("input should tokenize")
            .into_iter()
            .map(|token| token.kind)
            .collect()
    }

    #[test]
    fn tokenizes_identifiers_numbers_operators_and_punctuation() {
        assert_eq!(
            kinds("schema.events[id] = .5 + 12. - 3e-2 * 4 / 2 % 2;"),
            vec![
                TokenKind::Identifier("schema".into()),
                TokenKind::Punctuation(Punctuation::Dot),
                TokenKind::Identifier("events".into()),
                TokenKind::Punctuation(Punctuation::LeftBracket),
                TokenKind::Identifier("id".into()),
                TokenKind::Punctuation(Punctuation::RightBracket),
                TokenKind::Operator(Operator::Equal),
                TokenKind::Number(".5".into()),
                TokenKind::Operator(Operator::Plus),
                TokenKind::Number("12.".into()),
                TokenKind::Operator(Operator::Minus),
                TokenKind::Number("3e-2".into()),
                TokenKind::Operator(Operator::Multiply),
                TokenKind::Number("4".into()),
                TokenKind::Operator(Operator::Divide),
                TokenKind::Number("2".into()),
                TokenKind::Operator(Operator::Modulo),
                TokenKind::Number("2".into()),
                TokenKind::Semicolon,
            ]
        );
    }

    #[test]
    fn tokenizes_benchmark_shaped_multi_statement_batch_with_spans() {
        let sql = "CREATE TABLE events (ts Int64, user_id Int64, path String, value Float64);\n\
                   INSERT INTO events VALUES\n\
                   (1, 10, '/docs', 12.5),\n\
                   (2, 11, 'O''Reilly', -3.25e1);\n\
                   SELECT user_id, value * 2 FROM events\n\
                   WHERE value >= 10.0 AND path != 'bot''s' AND value < 1e3;";

        let tokens = tokenize(sql).expect("batch should tokenize");
        assert_eq!(
            tokens
                .iter()
                .filter(|token| token.kind == TokenKind::Semicolon)
                .count(),
            3
        );
        assert!(tokens.iter().any(|token| {
            token.kind == TokenKind::String("O'Reilly".into())
                && token.source(sql) == Some("'O''Reilly'")
        }));
        assert!(tokens.iter().any(|token| {
            token.kind == TokenKind::Operator(Operator::GreaterThanOrEqual)
                && token.source(sql) == Some(">=")
        }));
        assert!(tokens.iter().any(|token| {
            token.kind == TokenKind::Number("3.25e1".into()) && token.source(sql) == Some("3.25e1")
        }));

        for token in &tokens {
            let source = token.source(sql).expect("token span should be valid");
            assert!(!source.is_empty());
            assert_eq!(source, &sql[token.span.byte_range()]);
        }
        for pair in tokens.windows(2) {
            assert!(pair[0].span.end.byte_offset <= pair[1].span.start.byte_offset);
        }
    }

    #[test]
    fn decodes_empty_and_doubled_quote_strings() {
        assert_eq!(
            kinds("'' 'can''t' ''''"),
            vec![
                TokenKind::String(String::new()),
                TokenKind::String("can't".into()),
                TokenKind::String("'".into()),
            ]
        );
    }

    #[test]
    fn recognizes_comparison_and_concatenation_spellings() {
        assert_eq!(
            kinds("a == b AND b <> c AND c <= d AND d > e OR x || y != z"),
            vec![
                TokenKind::Identifier("a".into()),
                TokenKind::Operator(Operator::Equal),
                TokenKind::Identifier("b".into()),
                TokenKind::Identifier("AND".into()),
                TokenKind::Identifier("b".into()),
                TokenKind::Operator(Operator::NotEqual),
                TokenKind::Identifier("c".into()),
                TokenKind::Identifier("AND".into()),
                TokenKind::Identifier("c".into()),
                TokenKind::Operator(Operator::LessThanOrEqual),
                TokenKind::Identifier("d".into()),
                TokenKind::Identifier("AND".into()),
                TokenKind::Identifier("d".into()),
                TokenKind::Operator(Operator::GreaterThan),
                TokenKind::Identifier("e".into()),
                TokenKind::Identifier("OR".into()),
                TokenKind::Identifier("x".into()),
                TokenKind::Operator(Operator::Concatenate),
                TokenKind::Identifier("y".into()),
                TokenKind::Operator(Operator::NotEqual),
                TokenKind::Identifier("z".into()),
            ]
        );
    }

    #[test]
    fn skips_line_and_block_comments_without_counting_tokens() {
        let config = LexerConfig {
            max_input_bytes: 100,
            max_tokens: 2,
        };
        let tokens = tokenize_with_config("one -- two\n/* three */ four", config)
            .expect("comments should be trivia");
        assert_eq!(
            tokens
                .into_iter()
                .map(|token| token.kind)
                .collect::<Vec<_>>(),
            vec![
                TokenKind::Identifier("one".into()),
                TokenKind::Identifier("four".into())
            ]
        );
    }

    #[test]
    fn tracks_crlf_and_carriage_return_lines() {
        let sql = "one -- ignored\r\ntwo\rthree";
        let tokens = tokenize(sql).expect("input should tokenize");
        let positions = tokens
            .iter()
            .map(|token| (token.source(sql).unwrap(), token.span.start))
            .collect::<Vec<_>>();

        assert_eq!(positions[0].1.line, 1);
        assert_eq!(positions[1].1.line, 2);
        assert_eq!(positions[2].1.line, 3);
    }

    #[test]
    fn enforces_byte_limit_at_the_first_disallowed_byte() {
        let error = tokenize_with_config(
            "abé",
            LexerConfig {
                max_input_bytes: 3,
                max_tokens: 10,
            },
        )
        .expect_err("UTF-8 byte length should be bounded");

        assert_eq!(
            error,
            LexError::InputTooLarge {
                limit: 3,
                actual: 4,
                position: Position {
                    byte_offset: 3,
                    line: 1,
                    column: 3,
                },
            }
        );
    }

    #[test]
    fn permits_exact_limits_and_rejects_the_next_token() {
        let config = LexerConfig {
            max_input_bytes: 8,
            max_tokens: 1,
        };
        assert_eq!(tokenize_with_config("one", config).unwrap().len(), 1);

        let error = tokenize_with_config("one\n two", config)
            .expect_err("a second token should exceed the limit");
        assert_eq!(
            error,
            LexError::TokenLimitExceeded {
                limit: 1,
                position: Position {
                    byte_offset: 5,
                    line: 2,
                    column: 2,
                },
            }
        );
    }

    #[test]
    fn reports_malformed_input_with_typed_positions() {
        let cases = [
            (
                "SELECT 'broken",
                LexError::UnterminatedString {
                    position: Position {
                        byte_offset: 7,
                        line: 1,
                        column: 8,
                    },
                },
            ),
            (
                "1e+",
                LexError::InvalidNumber {
                    position: Position {
                        byte_offset: 1,
                        line: 1,
                        column: 2,
                    },
                },
            ),
            (
                "ok\n@bad",
                LexError::UnexpectedCharacter {
                    character: '@',
                    position: Position {
                        byte_offset: 3,
                        line: 2,
                        column: 1,
                    },
                },
            ),
            (
                "SELECT /* never closed",
                LexError::UnterminatedBlockComment {
                    position: Position {
                        byte_offset: 7,
                        line: 1,
                        column: 8,
                    },
                },
            ),
        ];

        for (input, expected) in cases {
            let error = tokenize(input).expect_err("input should be malformed");
            assert_eq!(error.position(), expected.position());
            assert_eq!(error, expected);
            assert!(!error.to_string().is_empty());
        }
    }
}
