//! Parsing and execution for constant `SELECT` statements.

use std::error::Error;
use std::fmt;

/// A scalar value produced by a constant query.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Int64(i64),
    Float64(f64),
    Bool(bool),
    String(String),
}

/// A named scalar result column.
#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    pub name: String,
    pub value: Value,
}

/// The single output row from one constant `SELECT` statement.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryResult {
    pub columns: Vec<Column>,
}

/// A syntax or literal-conversion error in SQL input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryError {
    offset: usize,
    message: String,
}

impl QueryError {
    fn new(offset: usize, message: impl Into<String>) -> Self {
        Self {
            offset,
            message: message.into(),
        }
    }

    /// Zero-based byte offset of the error in the SQL input.
    pub fn offset(&self) -> usize {
        self.offset
    }
}

impl fmt::Display for QueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "SQL error at byte {}: {}",
            self.offset, self.message
        )
    }
}

impl Error for QueryError {}

/// Executes semicolon-separated constant `SELECT` statements.
///
/// Each projection must be an `Int64`, `Float64`, `Bool`, or String literal.
/// An `AS identifier` alias is optional. No tables, clauses, or expressions
/// are accepted.
pub fn execute(sql: &str) -> Result<Vec<QueryResult>, QueryError> {
    Parser::new(sql)?.parse_all()
}

#[derive(Clone, Debug, PartialEq)]
enum TokenKind {
    Select,
    As,
    Int64(i64),
    Float64(f64),
    Bool(bool),
    String(String),
    Identifier(String),
    Comma,
    Semicolon,
    End,
}

#[derive(Clone, Debug, PartialEq)]
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

    fn next_token(&mut self) -> Result<Token, QueryError> {
        self.skip_whitespace();
        let start = self.position;
        let Some(character) = self.current_character() else {
            return Ok(Token {
                kind: TokenKind::End,
                start,
                end: start,
            });
        };

        match character {
            ',' => {
                self.position += 1;
                Ok(self.token(TokenKind::Comma, start))
            }
            ';' => {
                self.position += 1;
                Ok(self.token(TokenKind::Semicolon, start))
            }
            '\'' => self.lex_string(),
            '+' | '-' if self.next_character_is_ascii_digit() => self.lex_number(),
            '.' if self.next_character_is_ascii_digit() => self.lex_number(),
            character if character.is_ascii_digit() => self.lex_number(),
            character if is_identifier_start(character) => self.lex_identifier(),
            _ => Err(QueryError::new(
                start,
                format!("unexpected character {character:?}"),
            )),
        }
    }

    fn lex_identifier(&mut self) -> Result<Token, QueryError> {
        let start = self.position;
        self.advance_character();
        while self.current_character().is_some_and(is_identifier_continue) {
            self.advance_character();
        }

        let text = &self.sql[start..self.position];
        let kind = if text.eq_ignore_ascii_case("SELECT") {
            TokenKind::Select
        } else if text.eq_ignore_ascii_case("AS") {
            TokenKind::As
        } else if text.eq_ignore_ascii_case("TRUE") {
            TokenKind::Bool(true)
        } else if text.eq_ignore_ascii_case("FALSE") {
            TokenKind::Bool(false)
        } else {
            TokenKind::Identifier(text.to_owned())
        };

        Ok(self.token(kind, start))
    }

    fn lex_number(&mut self) -> Result<Token, QueryError> {
        let start = self.position;
        if matches!(self.current_character(), Some('+' | '-')) {
            self.advance_character();
        }

        let mut is_float = false;
        if self.current_character() == Some('.') {
            is_float = true;
            self.advance_character();
            self.consume_ascii_digits();
        } else {
            self.consume_ascii_digits();
            if self.current_character() == Some('.') {
                is_float = true;
                self.advance_character();
                self.consume_ascii_digits();
            }
        }

        if matches!(self.current_character(), Some('e' | 'E')) {
            is_float = true;
            let exponent_offset = self.position;
            self.advance_character();
            if matches!(self.current_character(), Some('+' | '-')) {
                self.advance_character();
            }
            if !self
                .current_character()
                .is_some_and(|character| character.is_ascii_digit())
            {
                return Err(QueryError::new(
                    exponent_offset,
                    "a decimal exponent requires at least one digit",
                ));
            }
            self.consume_ascii_digits();
        }

        let text = &self.sql[start..self.position];
        let kind = if is_float {
            let value = text
                .parse::<f64>()
                .map_err(|_| QueryError::new(start, format!("invalid Float64 literal {text:?}")))?;
            if !value.is_finite() {
                return Err(QueryError::new(
                    start,
                    format!("Float64 literal {text:?} is outside the finite range"),
                ));
            }
            TokenKind::Float64(value)
        } else {
            let value = text.parse::<i64>().map_err(|_| {
                QueryError::new(start, format!("Int64 literal {text:?} is out of range"))
            })?;
            TokenKind::Int64(value)
        };

        Ok(self.token(kind, start))
    }

    fn lex_string(&mut self) -> Result<Token, QueryError> {
        let start = self.position;
        self.position += 1;
        let mut value = String::new();

        loop {
            let Some(character) = self.current_character() else {
                return Err(QueryError::new(start, "unterminated String literal"));
            };

            match character {
                '\'' => {
                    self.position += 1;
                    if self.current_character() == Some('\'') {
                        value.push('\'');
                        self.position += 1;
                    } else {
                        return Ok(self.token(TokenKind::String(value), start));
                    }
                }
                '\\' => {
                    let escape_offset = self.position;
                    self.position += 1;
                    let Some(escaped) = self.current_character() else {
                        return Err(QueryError::new(start, "unterminated String literal"));
                    };
                    self.advance_character();
                    match escaped {
                        '\\' => value.push('\\'),
                        '\'' => value.push('\''),
                        '"' => value.push('"'),
                        'n' => value.push('\n'),
                        'r' => value.push('\r'),
                        't' => value.push('\t'),
                        '0' => value.push('\0'),
                        _ => {
                            return Err(QueryError::new(
                                escape_offset,
                                format!("unsupported String escape \\{escaped}"),
                            ));
                        }
                    }
                }
                _ => {
                    value.push(character);
                    self.advance_character();
                }
            }
        }
    }

    fn consume_ascii_digits(&mut self) {
        while self
            .current_character()
            .is_some_and(|character| character.is_ascii_digit())
        {
            self.advance_character();
        }
    }

    fn skip_whitespace(&mut self) {
        while self.current_character().is_some_and(char::is_whitespace) {
            self.advance_character();
        }
    }

    fn current_character(&self) -> Option<char> {
        self.sql[self.position..].chars().next()
    }

    fn next_character_is_ascii_digit(&self) -> bool {
        let mut characters = self.sql[self.position..].chars();
        characters.next();
        characters
            .next()
            .is_some_and(|character| character.is_ascii_digit())
    }

    fn advance_character(&mut self) {
        if let Some(character) = self.current_character() {
            self.position += character.len_utf8();
        }
    }

    fn token(&self, kind: TokenKind, start: usize) -> Token {
        Token {
            kind,
            start,
            end: self.position,
        }
    }
}

struct Parser<'a> {
    sql: &'a str,
    lexer: Lexer<'a>,
    current: Token,
}

impl<'a> Parser<'a> {
    fn new(sql: &'a str) -> Result<Self, QueryError> {
        let mut lexer = Lexer::new(sql);
        let current = lexer.next_token()?;
        Ok(Self {
            sql,
            lexer,
            current,
        })
    }

    fn parse_all(mut self) -> Result<Vec<QueryResult>, QueryError> {
        if self.current.kind == TokenKind::End {
            return Err(QueryError::new(0, "expected SELECT, found end of input"));
        }

        let mut results = Vec::new();
        loop {
            results.push(self.parse_select()?);
            match self.current.kind {
                TokenKind::Semicolon => {
                    self.advance()?;
                    if self.current.kind == TokenKind::End {
                        break;
                    }
                }
                TokenKind::End => break,
                _ => {
                    return Err(self.expected("',' or ';' or end of input"));
                }
            }
        }
        Ok(results)
    }

    fn parse_select(&mut self) -> Result<QueryResult, QueryError> {
        if self.current.kind != TokenKind::Select {
            return Err(self.expected("SELECT"));
        }
        self.advance()?;

        let mut columns = Vec::new();
        loop {
            let literal = self.current.clone();
            let value = match literal.kind {
                TokenKind::Int64(value) => Value::Int64(value),
                TokenKind::Float64(value) => Value::Float64(value),
                TokenKind::Bool(value) => Value::Bool(value),
                TokenKind::String(value) => Value::String(value),
                _ => return Err(self.expected("an Int64, Float64, Bool, or String literal")),
            };
            let mut name = self.sql[literal.start..literal.end].to_owned();
            self.advance()?;

            if self.current.kind == TokenKind::As {
                self.advance()?;
                match &self.current.kind {
                    TokenKind::Identifier(alias) => name.clone_from(alias),
                    _ => return Err(self.expected("an identifier after AS")),
                }
                self.advance()?;
            }

            columns.push(Column { name, value });
            if self.current.kind != TokenKind::Comma {
                break;
            }
            self.advance()?;
        }

        Ok(QueryResult { columns })
    }

    fn advance(&mut self) -> Result<(), QueryError> {
        self.current = self.lexer.next_token()?;
        Ok(())
    }

    fn expected(&self, expected: &str) -> QueryError {
        QueryError::new(
            self.current.start,
            format!(
                "expected {expected}, found {}",
                token_description(&self.current.kind)
            ),
        )
    }
}

fn token_description(token: &TokenKind) -> String {
    match token {
        TokenKind::Select => "SELECT".to_owned(),
        TokenKind::As => "AS".to_owned(),
        TokenKind::Int64(value) => format!("Int64 literal {value}"),
        TokenKind::Float64(value) => format!("Float64 literal {value}"),
        TokenKind::Bool(value) => format!("Bool literal {value}"),
        TokenKind::String(_) => "String literal".to_owned(),
        TokenKind::Identifier(identifier) => format!("identifier {identifier:?}"),
        TokenKind::Comma => "','".to_owned(),
        TokenKind::Semicolon => "';'".to_owned(),
        TokenKind::End => "end of input".to_owned(),
    }
}

fn is_identifier_start(character: char) -> bool {
    character.is_ascii_alphabetic() || character == '_'
}

fn is_identifier_continue(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_typed_aliased_literals() {
        let results =
            execute("SeLeCt -42 AS integer, +1.25e2 AS floating, TRUE AS flag, 'it''s' AS text;")
                .unwrap();

        assert_eq!(
            results,
            vec![QueryResult {
                columns: vec![
                    Column {
                        name: "integer".to_owned(),
                        value: Value::Int64(-42),
                    },
                    Column {
                        name: "floating".to_owned(),
                        value: Value::Float64(125.0),
                    },
                    Column {
                        name: "flag".to_owned(),
                        value: Value::Bool(true),
                    },
                    Column {
                        name: "text".to_owned(),
                        value: Value::String("it's".to_owned()),
                    },
                ],
            }]
        );
    }

    #[test]
    fn supports_multiple_statements_and_unaliased_names() {
        let results = execute("SELECT 1; SELECT 'line\\nquote\\'';").unwrap();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].columns[0].name, "1");
        assert_eq!(results[1].columns[0].name, "'line\\nquote\\''");
        assert_eq!(
            results[1].columns[0].value,
            Value::String("line\nquote'".to_owned())
        );
    }

    #[test]
    fn rejects_non_literal_sql_and_invalid_literals() {
        for sql in [
            "SELECT 1 FROM table_name",
            "SELECT 1 + 2",
            "SELECT count()",
            "SELECT 9223372036854775808",
            "SELECT 1e999",
            "SELECT 'bad\\xescape'",
            "SELECT 'unterminated",
            "SELECT 1;; SELECT 2",
        ] {
            assert!(execute(sql).is_err(), "{sql:?} should fail");
        }
    }
}
