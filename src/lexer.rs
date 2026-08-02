//! A bounded lexer for RustHouse's SQL surface.
//!
//! Tokens own their values so they can outlive the query buffer. Their spans
//! are half-open byte offsets into the original UTF-8 input.

use std::error::Error;
use std::fmt;
use std::ops::Range;

/// A half-open byte range in the SQL input.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
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

    pub const fn as_range(self) -> Range<usize> {
        self.start..self.end
    }
}

/// Resource limits applied to one call to [`lex`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LexerLimits {
    /// Maximum UTF-8 input size in bytes.
    pub max_input_bytes: usize,
    /// Maximum number of emitted tokens.
    pub max_tokens: usize,
    /// Maximum decoded string or source numeric literal size in bytes.
    pub max_literal_bytes: usize,
}

impl LexerLimits {
    pub const fn new(max_input_bytes: usize, max_tokens: usize, max_literal_bytes: usize) -> Self {
        Self {
            max_input_bytes,
            max_tokens,
            max_literal_bytes,
        }
    }
}

impl Default for LexerLimits {
    fn default() -> Self {
        Self::new(1024 * 1024, 100_000, 1024 * 1024)
    }
}

/// A lexical token and its byte span in the original input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

/// SQL token categories.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TokenKind {
    /// An unquoted or decoded double-quoted identifier.
    Identifier(String),
    Literal(Literal),
    Operator(Operator),
    Delimiter(Delimiter),
}

/// SQL literal values recognized without imposing later type semantics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Literal {
    /// The source spelling of an integer, decimal, or exponent-form number.
    Number(String),
    /// A decoded single-quoted string. Two adjacent quotes decode to one quote.
    String(String),
    Boolean(bool),
    Null,
}

/// Symbolic SQL operators.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operator {
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
    Plus,
    Minus,
    Multiply,
    Divide,
    Modulo,
    Concat,
    Cast,
}

/// Punctuation that separates SQL expressions and statements.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Delimiter {
    LeftParenthesis,
    RightParenthesis,
    LeftBracket,
    RightBracket,
    Comma,
    Dot,
    Semicolon,
}

/// A lexer failure with a zero-based byte position.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LexError {
    pub kind: LexErrorKind,
    pub position: usize,
}

impl LexError {
    const fn new(kind: LexErrorKind, position: usize) -> Self {
        Self { kind, position }
    }
}

/// Typed reasons that lexing can fail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LexErrorKind {
    InputTooLarge { length: usize, limit: usize },
    TokenLimitExceeded { limit: usize },
    LiteralTooLarge { length: usize, limit: usize },
    UnterminatedString,
    UnterminatedQuotedIdentifier,
    UnterminatedBlockComment,
    InvalidNumber,
    UnexpectedCharacter(char),
}

impl fmt::Display for LexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "SQL lexer error at byte {}: ", self.position)?;
        match self.kind {
            LexErrorKind::InputTooLarge { length, limit } => {
                write!(formatter, "input has {length} bytes, limit is {limit}")
            }
            LexErrorKind::TokenLimitExceeded { limit } => {
                write!(formatter, "token limit of {limit} exceeded")
            }
            LexErrorKind::LiteralTooLarge { length, limit } => {
                write!(formatter, "literal has {length} bytes, limit is {limit}")
            }
            LexErrorKind::UnterminatedString => formatter.write_str("unterminated string literal"),
            LexErrorKind::UnterminatedQuotedIdentifier => {
                formatter.write_str("unterminated quoted identifier")
            }
            LexErrorKind::UnterminatedBlockComment => {
                formatter.write_str("unterminated block comment")
            }
            LexErrorKind::InvalidNumber => formatter.write_str("invalid numeric literal"),
            LexErrorKind::UnexpectedCharacter(character) => {
                write!(formatter, "unexpected character {character:?}")
            }
        }
    }
}

impl Error for LexError {}

/// Tokenizes SQL while enforcing all supplied resource limits.
///
/// Whitespace and `--` or nested `/* ... */` comments are skipped. Keywords
/// remain identifiers so a later parser can apply its own dialect and reserved
/// word rules. `TRUE`, `FALSE`, and `NULL` are recognized as literals.
pub fn lex(input: &str, limits: LexerLimits) -> Result<Vec<Token>, LexError> {
    if input.len() > limits.max_input_bytes {
        return Err(LexError::new(
            LexErrorKind::InputTooLarge {
                length: input.len(),
                limit: limits.max_input_bytes,
            },
            limits.max_input_bytes,
        ));
    }

    Lexer {
        input,
        cursor: 0,
        limits,
        tokens: Vec::new(),
    }
    .run()
}

struct Lexer<'a> {
    input: &'a str,
    cursor: usize,
    limits: LexerLimits,
    tokens: Vec<Token>,
}

impl Lexer<'_> {
    fn run(mut self) -> Result<Vec<Token>, LexError> {
        while self.cursor < self.input.len() {
            self.skip_trivia()?;
            if self.cursor == self.input.len() {
                break;
            }

            let start = self.cursor;
            let character = self.current_char();
            if character == '\'' {
                self.string_literal()?;
            } else if character == '"' {
                self.quoted_identifier()?;
            } else if character.is_ascii_digit()
                || (character == '.' && self.next_char().is_some_and(|next| next.is_ascii_digit()))
            {
                self.number()?;
            } else if is_identifier_start(character) {
                self.identifier()?;
            } else {
                self.symbol(character, start)?;
            }
        }

        Ok(self.tokens)
    }

    fn skip_trivia(&mut self) -> Result<(), LexError> {
        loop {
            while self.cursor < self.input.len() && self.current_char().is_whitespace() {
                self.advance_char();
            }

            if self.remaining().starts_with("--") {
                self.cursor += 2;
                while self.cursor < self.input.len() {
                    let character = self.current_char();
                    self.advance_char();
                    if character == '\n' || character == '\r' {
                        break;
                    }
                }
            } else if self.remaining().starts_with("/*") {
                self.block_comment()?;
            } else {
                return Ok(());
            }
        }
    }

    fn block_comment(&mut self) -> Result<(), LexError> {
        let start = self.cursor;
        self.cursor += 2;
        let mut depth = 1usize;

        while self.cursor < self.input.len() {
            if self.remaining().starts_with("/*") {
                depth += 1;
                self.cursor += 2;
            } else if self.remaining().starts_with("*/") {
                depth -= 1;
                self.cursor += 2;
                if depth == 0 {
                    return Ok(());
                }
            } else {
                self.advance_char();
            }
        }

        Err(LexError::new(LexErrorKind::UnterminatedBlockComment, start))
    }

    fn string_literal(&mut self) -> Result<(), LexError> {
        let start = self.cursor;
        self.cursor += 1;
        let content_start = self.cursor;
        let mut decoded_length = 0usize;

        while self.cursor < self.input.len() {
            let character = self.current_char();
            if character == '\'' {
                if self.remaining().starts_with("''") {
                    decoded_length += 1;
                    self.cursor += 2;
                    continue;
                }

                let content_end = self.cursor;
                self.cursor += 1;
                if decoded_length > self.limits.max_literal_bytes {
                    return Err(LexError::new(
                        LexErrorKind::LiteralTooLarge {
                            length: decoded_length,
                            limit: self.limits.max_literal_bytes,
                        },
                        start,
                    ));
                }

                let value = decode_doubled_quotes(
                    &self.input[content_start..content_end],
                    '\'',
                    decoded_length,
                );
                return self.push(TokenKind::Literal(Literal::String(value)), start);
            }

            decoded_length += character.len_utf8();
            self.advance_char();
        }

        Err(LexError::new(LexErrorKind::UnterminatedString, start))
    }

    fn quoted_identifier(&mut self) -> Result<(), LexError> {
        let start = self.cursor;
        self.cursor += 1;
        let content_start = self.cursor;
        let mut decoded_length = 0usize;

        while self.cursor < self.input.len() {
            let character = self.current_char();
            if character == '"' {
                if self.remaining().starts_with("\"\"") {
                    decoded_length += 1;
                    self.cursor += 2;
                    continue;
                }

                let content_end = self.cursor;
                self.cursor += 1;
                let value = decode_doubled_quotes(
                    &self.input[content_start..content_end],
                    '"',
                    decoded_length,
                );
                return self.push(TokenKind::Identifier(value), start);
            }

            decoded_length += character.len_utf8();
            self.advance_char();
        }

        Err(LexError::new(
            LexErrorKind::UnterminatedQuotedIdentifier,
            start,
        ))
    }

    fn number(&mut self) -> Result<(), LexError> {
        let start = self.cursor;
        let starts_with_dot = self.current_char() == '.';
        if starts_with_dot {
            self.cursor += 1;
            self.consume_ascii_digits();
        } else {
            self.consume_ascii_digits();
            if self.remaining().starts_with('.') {
                self.cursor += 1;
                self.consume_ascii_digits();
            }
        }

        if self
            .remaining()
            .as_bytes()
            .first()
            .is_some_and(|byte| *byte == b'e' || *byte == b'E')
        {
            let exponent_position = self.cursor;
            self.cursor += 1;
            if self
                .remaining()
                .as_bytes()
                .first()
                .is_some_and(|byte| *byte == b'+' || *byte == b'-')
            {
                self.cursor += 1;
            }
            if !self
                .remaining()
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_digit)
            {
                return Err(LexError::new(
                    LexErrorKind::InvalidNumber,
                    exponent_position,
                ));
            }
            self.consume_ascii_digits();
        }

        if self.cursor < self.input.len() && is_identifier_start(self.current_char()) {
            return Err(LexError::new(LexErrorKind::InvalidNumber, self.cursor));
        }

        let length = self.cursor - start;
        if length > self.limits.max_literal_bytes {
            return Err(LexError::new(
                LexErrorKind::LiteralTooLarge {
                    length,
                    limit: self.limits.max_literal_bytes,
                },
                start,
            ));
        }

        let value = self.input[start..self.cursor].to_owned();
        self.push(TokenKind::Literal(Literal::Number(value)), start)
    }

    fn identifier(&mut self) -> Result<(), LexError> {
        let start = self.cursor;
        self.advance_char();
        while self.cursor < self.input.len() && is_identifier_continue(self.current_char()) {
            self.advance_char();
        }

        let value = &self.input[start..self.cursor];
        let kind = if value.eq_ignore_ascii_case("true") {
            self.check_literal_length(start, value.len())?;
            TokenKind::Literal(Literal::Boolean(true))
        } else if value.eq_ignore_ascii_case("false") {
            self.check_literal_length(start, value.len())?;
            TokenKind::Literal(Literal::Boolean(false))
        } else if value.eq_ignore_ascii_case("null") {
            self.check_literal_length(start, value.len())?;
            TokenKind::Literal(Literal::Null)
        } else {
            TokenKind::Identifier(value.to_owned())
        };
        self.push(kind, start)
    }

    fn check_literal_length(&self, start: usize, length: usize) -> Result<(), LexError> {
        if length > self.limits.max_literal_bytes {
            return Err(LexError::new(
                LexErrorKind::LiteralTooLarge {
                    length,
                    limit: self.limits.max_literal_bytes,
                },
                start,
            ));
        }
        Ok(())
    }

    fn symbol(&mut self, character: char, start: usize) -> Result<(), LexError> {
        let (kind, width) =
            if self.remaining().starts_with("!=") || self.remaining().starts_with("<>") {
                (TokenKind::Operator(Operator::NotEqual), 2)
            } else if self.remaining().starts_with("<=") {
                (TokenKind::Operator(Operator::LessOrEqual), 2)
            } else if self.remaining().starts_with(">=") {
                (TokenKind::Operator(Operator::GreaterOrEqual), 2)
            } else if self.remaining().starts_with("||") {
                (TokenKind::Operator(Operator::Concat), 2)
            } else if self.remaining().starts_with("::") {
                (TokenKind::Operator(Operator::Cast), 2)
            } else {
                let kind = match character {
                    '=' => TokenKind::Operator(Operator::Equal),
                    '<' => TokenKind::Operator(Operator::Less),
                    '>' => TokenKind::Operator(Operator::Greater),
                    '+' => TokenKind::Operator(Operator::Plus),
                    '-' => TokenKind::Operator(Operator::Minus),
                    '*' => TokenKind::Operator(Operator::Multiply),
                    '/' => TokenKind::Operator(Operator::Divide),
                    '%' => TokenKind::Operator(Operator::Modulo),
                    '(' => TokenKind::Delimiter(Delimiter::LeftParenthesis),
                    ')' => TokenKind::Delimiter(Delimiter::RightParenthesis),
                    '[' => TokenKind::Delimiter(Delimiter::LeftBracket),
                    ']' => TokenKind::Delimiter(Delimiter::RightBracket),
                    ',' => TokenKind::Delimiter(Delimiter::Comma),
                    '.' => TokenKind::Delimiter(Delimiter::Dot),
                    ';' => TokenKind::Delimiter(Delimiter::Semicolon),
                    _ => {
                        return Err(LexError::new(
                            LexErrorKind::UnexpectedCharacter(character),
                            start,
                        ));
                    }
                };
                (kind, character.len_utf8())
            };

        self.cursor += width;
        self.push(kind, start)
    }

    fn push(&mut self, kind: TokenKind, start: usize) -> Result<(), LexError> {
        if self.tokens.len() >= self.limits.max_tokens {
            return Err(LexError::new(
                LexErrorKind::TokenLimitExceeded {
                    limit: self.limits.max_tokens,
                },
                start,
            ));
        }
        self.tokens.push(Token {
            kind,
            span: Span::new(start, self.cursor),
        });
        Ok(())
    }

    fn consume_ascii_digits(&mut self) {
        while self
            .remaining()
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_digit)
        {
            self.cursor += 1;
        }
    }

    fn remaining(&self) -> &str {
        &self.input[self.cursor..]
    }

    fn current_char(&self) -> char {
        self.remaining()
            .chars()
            .next()
            .expect("the cursor is within the input")
    }

    fn next_char(&self) -> Option<char> {
        self.remaining().chars().nth(1)
    }

    fn advance_char(&mut self) {
        self.cursor += self.current_char().len_utf8();
    }
}

fn is_identifier_start(character: char) -> bool {
    character == '_' || character.is_alphabetic()
}

fn is_identifier_continue(character: char) -> bool {
    character == '_' || character == '$' || character.is_alphanumeric()
}

fn decode_doubled_quotes(input: &str, quote: char, capacity: usize) -> String {
    let mut decoded = String::with_capacity(capacity);
    let mut characters = input.chars().peekable();
    while let Some(character) = characters.next() {
        decoded.push(character);
        if character == quote && characters.peek() == Some(&quote) {
            characters.next();
        }
    }
    decoded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generous_limits() -> LexerLimits {
        LexerLimits::new(16 * 1024, 1024, 1024)
    }

    #[test]
    fn tokenizes_benchmark_shaped_query_with_byte_spans() {
        let sql = "SELECT region, SUM(revenue) AS total\n\
                   FROM sales /* benchmark fact table */\n\
                   WHERE day >= '2026-01-01' AND note = 'customer''s'\n\
                   GROUP BY region ORDER BY total DESC LIMIT 10;";
        let tokens = lex(sql, generous_limits()).unwrap();

        assert_eq!(
            tokens.first(),
            Some(&Token {
                kind: TokenKind::Identifier("SELECT".into()),
                span: Span::new(0, 6),
            })
        );
        assert!(tokens.iter().any(|token| {
            token.kind == TokenKind::Operator(Operator::GreaterOrEqual)
                && &sql[token.span.as_range()] == ">="
        }));
        assert!(tokens.iter().any(|token| {
            token.kind == TokenKind::Delimiter(Delimiter::LeftParenthesis)
                && &sql[token.span.as_range()] == "("
        }));
        assert!(tokens.iter().any(|token| {
            token.kind == TokenKind::Literal(Literal::String("customer's".into()))
                && &sql[token.span.as_range()] == "'customer''s'"
        }));
        assert_eq!(
            tokens.last().map(|token| &token.kind),
            Some(&TokenKind::Delimiter(Delimiter::Semicolon))
        );
        for token in &tokens {
            assert!(sql.is_char_boundary(token.span.start));
            assert!(sql.is_char_boundary(token.span.end));
            assert!(!&sql[token.span.as_range()].is_empty());
        }
    }

    #[test]
    fn decodes_quoted_identifiers_strings_and_unicode_spans() {
        let sql = "SELECT \"daily\"\"total\", 'café''s', .5e+2, TRUE, false, NULL";
        let tokens = lex(sql, generous_limits()).unwrap();

        assert!(tokens.iter().any(|token| {
            token.kind == TokenKind::Identifier("daily\"total".into())
                && &sql[token.span.as_range()] == "\"daily\"\"total\""
        }));
        assert!(tokens.iter().any(|token| {
            token.kind == TokenKind::Literal(Literal::String("café's".into()))
                && &sql[token.span.as_range()] == "'café''s'"
        }));
        assert!(
            tokens
                .iter()
                .any(|token| token.kind == TokenKind::Literal(Literal::Number(".5e+2".into())))
        );
        assert!(
            tokens
                .iter()
                .any(|token| token.kind == TokenKind::Literal(Literal::Boolean(true)))
        );
        assert!(
            tokens
                .iter()
                .any(|token| token.kind == TokenKind::Literal(Literal::Boolean(false)))
        );
        assert!(
            tokens
                .iter()
                .any(|token| token.kind == TokenKind::Literal(Literal::Null))
        );
    }

    #[test]
    fn reports_typed_errors_at_byte_positions() {
        let cases = [
            ("'not closed", LexErrorKind::UnterminatedString, 0),
            (
                "select \"not closed",
                LexErrorKind::UnterminatedQuotedIdentifier,
                7,
            ),
            (
                "select /* nested /* comment */",
                LexErrorKind::UnterminatedBlockComment,
                7,
            ),
            ("select 1e+", LexErrorKind::InvalidNumber, 8),
            ("select @", LexErrorKind::UnexpectedCharacter('@'), 7),
        ];

        for (sql, kind, position) in cases {
            let error = lex(sql, generous_limits()).unwrap_err();
            assert_eq!(error.kind, kind);
            assert_eq!(error.position, position);
            assert!(error.to_string().contains(&format!("byte {position}")));
        }
    }

    #[test]
    fn enforces_input_limit_at_the_exact_boundary() {
        let limits = LexerLimits::new(6, 1, 6);
        assert_eq!(lex("select", limits).unwrap().len(), 1);

        assert_eq!(
            lex("select ", limits).unwrap_err(),
            LexError {
                kind: LexErrorKind::InputTooLarge {
                    length: 7,
                    limit: 6,
                },
                position: 6,
            }
        );
    }

    #[test]
    fn enforces_token_limit_at_the_exact_boundary() {
        let limits = LexerLimits::new(16, 1, 16);
        assert_eq!(lex("a", limits).unwrap().len(), 1);
        assert_eq!(
            lex("a b", limits).unwrap_err(),
            LexError {
                kind: LexErrorKind::TokenLimitExceeded { limit: 1 },
                position: 2,
            }
        );
        assert!(
            lex("  -- no tokens", LexerLimits::new(32, 0, 0))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn enforces_decoded_literal_limit_at_the_exact_boundary() {
        let limits = LexerLimits::new(32, 1, 3);
        assert_eq!(
            lex("'a''b'", limits).unwrap()[0].kind,
            TokenKind::Literal(Literal::String("a'b".into()))
        );

        assert_eq!(
            lex("'a''bc'", limits).unwrap_err(),
            LexError {
                kind: LexErrorKind::LiteralTooLarge {
                    length: 4,
                    limit: 3,
                },
                position: 0,
            }
        );
        assert!(lex("123", limits).is_ok());
        assert!(matches!(
            lex("1234", limits).unwrap_err().kind,
            LexErrorKind::LiteralTooLarge {
                length: 4,
                limit: 3
            }
        ));
    }
}
