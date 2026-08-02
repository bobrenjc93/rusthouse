//! Tokenization and parsing for the supported SQL subset.

use crate::{ColumnSchema, DataType, Error, InsertError, Result, Value};

/// A parsed SQL statement.
#[derive(Clone, Debug, PartialEq)]
pub enum Statement {
    CreateTable(CreateTable),
    Insert(Insert),
}

/// A parsed `CREATE TABLE` statement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateTable {
    name: String,
    columns: Vec<ColumnSchema>,
}

impl CreateTable {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn columns(&self) -> &[ColumnSchema] {
        &self.columns
    }

    pub(crate) fn into_parts(self) -> (String, Vec<ColumnSchema>) {
        (self.name, self.columns)
    }
}

/// A parsed `INSERT INTO ... VALUES` statement.
#[derive(Clone, Debug, PartialEq)]
pub struct Insert {
    table_name: String,
    rows: Vec<Vec<Value>>,
}

impl Insert {
    #[must_use]
    pub fn table_name(&self) -> &str {
        &self.table_name
    }

    #[must_use]
    pub fn rows(&self) -> &[Vec<Value>] {
        &self.rows
    }

    pub(crate) fn into_parts(self) -> (String, Vec<Vec<Value>>) {
        (self.table_name, self.rows)
    }
}

/// Parse exactly one supported SQL statement.
///
/// The byte-size bound is enforced by [`crate::Database`]. This function
/// enforces the caller-provided schema and insertion batch bounds.
pub fn parse_statement(
    input: &str,
    max_columns: usize,
    max_batch_rows: usize,
) -> Result<Statement> {
    let tokens = Lexer::new(input).tokenize()?;
    Parser::new(tokens, max_columns, max_batch_rows).parse_statement()
}

/// Parse exactly one `CREATE TABLE` statement.
///
/// The byte-size bound is enforced by [`crate::Database`]; this function
/// enforces the caller-provided column bound while parsing.
pub fn parse_create_table(input: &str, max_columns: usize) -> Result<CreateTable> {
    let tokens = Lexer::new(input).tokenize()?;
    Parser::new(tokens, max_columns, 0).parse_create_table()
}

#[derive(Clone, Debug, PartialEq)]
struct Token {
    kind: TokenKind,
    position: usize,
}

#[derive(Clone, Debug, PartialEq)]
enum TokenKind {
    Identifier(String),
    Number(String),
    String(String),
    Comma,
    LeftParenthesis,
    RightParenthesis,
    Semicolon,
    End,
}

struct Lexer<'a> {
    input: &'a str,
    position: usize,
}

impl<'a> Lexer<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, position: 0 }
    }

    fn tokenize(mut self) -> Result<Vec<Token>> {
        let mut tokens = Vec::new();
        loop {
            self.skip_whitespace();
            let position = self.position;
            let Some(character) = self.current() else {
                tokens.push(Token {
                    kind: TokenKind::End,
                    position,
                });
                return Ok(tokens);
            };

            let kind = match character {
                ',' => {
                    self.advance();
                    TokenKind::Comma
                }
                '(' => {
                    self.advance();
                    TokenKind::LeftParenthesis
                }
                ')' => {
                    self.advance();
                    TokenKind::RightParenthesis
                }
                ';' => {
                    self.advance();
                    TokenKind::Semicolon
                }
                '\'' => TokenKind::String(self.scan_string(position)?),
                character if character.is_ascii_alphabetic() || character == '_' => {
                    TokenKind::Identifier(self.scan_identifier())
                }
                character if character.is_ascii_digit() || self.number_starts_here() => {
                    TokenKind::Number(self.scan_number()?)
                }
                _ => {
                    return Err(Error::Syntax {
                        position,
                        message: format!("unexpected character {character:?}"),
                    });
                }
            };
            tokens.push(Token { kind, position });
        }
    }

    fn current(&self) -> Option<char> {
        self.input[self.position..].chars().next()
    }

    fn next(&self) -> Option<char> {
        let mut characters = self.input[self.position..].chars();
        characters.next()?;
        characters.next()
    }

    fn number_starts_here(&self) -> bool {
        match (self.current(), self.next()) {
            (Some('+' | '-'), Some(next)) => {
                next.is_ascii_digit()
                    || (next == '.'
                        && self.input[self.position + 2..]
                            .chars()
                            .next()
                            .is_some_and(|character| character.is_ascii_digit()))
            }
            (Some('.'), Some(next)) => next.is_ascii_digit(),
            _ => false,
        }
    }

    fn advance(&mut self) {
        if let Some(character) = self.current() {
            self.position += character.len_utf8();
        }
    }

    fn skip_whitespace(&mut self) {
        while self.current().is_some_and(char::is_whitespace) {
            self.advance();
        }
    }

    fn scan_identifier(&mut self) -> String {
        let start = self.position;
        while self
            .current()
            .is_some_and(|character| character.is_ascii_alphanumeric() || character == '_')
        {
            self.advance();
        }
        self.input[start..self.position].to_owned()
    }

    fn scan_number(&mut self) -> Result<String> {
        let start = self.position;
        if matches!(self.current(), Some('+' | '-')) {
            self.advance();
        }

        let mut digits = 0;
        while self.current().is_some_and(|value| value.is_ascii_digit()) {
            digits += 1;
            self.advance();
        }

        if matches!(self.current(), Some('.')) {
            self.advance();
            while self.current().is_some_and(|value| value.is_ascii_digit()) {
                digits += 1;
                self.advance();
            }
        }

        debug_assert!(digits > 0, "the caller recognizes numeric prefixes");

        if matches!(self.current(), Some('e' | 'E')) {
            self.advance();
            if matches!(self.current(), Some('+' | '-')) {
                self.advance();
            }
            let exponent_start = self.position;
            while self.current().is_some_and(|value| value.is_ascii_digit()) {
                self.advance();
            }
            if self.position == exponent_start {
                return Err(Error::Syntax {
                    position: self.position,
                    message: "expected exponent digits".to_owned(),
                });
            }
        }

        Ok(self.input[start..self.position].to_owned())
    }

    fn scan_string(&mut self, start: usize) -> Result<String> {
        self.advance();
        let mut value = String::new();
        loop {
            match self.current() {
                Some('\'') => {
                    self.advance();
                    if matches!(self.current(), Some('\'')) {
                        value.push('\'');
                        self.advance();
                    } else {
                        return Ok(value);
                    }
                }
                Some(character) => {
                    value.push(character);
                    self.advance();
                }
                None => {
                    return Err(Error::Syntax {
                        position: start,
                        message: "unterminated string literal".to_owned(),
                    });
                }
            }
        }
    }
}

struct Parser {
    tokens: Vec<Token>,
    position: usize,
    max_columns: usize,
    max_batch_rows: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>, max_columns: usize, max_batch_rows: usize) -> Self {
        Self {
            tokens,
            position: 0,
            max_columns,
            max_batch_rows,
        }
    }

    fn parse_statement(mut self) -> Result<Statement> {
        if self.current_is_keyword("CREATE") {
            self.parse_create_table_body().map(Statement::CreateTable)
        } else if self.current_is_keyword("INSERT") {
            self.parse_insert_body().map(Statement::Insert)
        } else {
            self.syntax("expected CREATE TABLE or INSERT INTO")
        }
    }

    fn parse_create_table(mut self) -> Result<CreateTable> {
        self.parse_create_table_body()
    }

    fn parse_create_table_body(&mut self) -> Result<CreateTable> {
        self.expect_keyword("CREATE")?;
        self.expect_keyword("TABLE")?;
        let name = self.expect_identifier("a table name")?.0;
        self.expect_symbol(TokenKind::LeftParenthesis, "'('")?;

        if matches!(self.current().kind, TokenKind::RightParenthesis) {
            return self.syntax("a table must contain at least one column");
        }

        let mut columns = Vec::new();
        loop {
            let (name, _) = self.expect_identifier("a column name")?;
            let (type_name, type_position) = self.expect_identifier("a column type")?;
            let data_type = DataType::parse(&type_name).ok_or(Error::UnknownType {
                name: type_name,
                position: type_position,
            })?;

            let actual = columns.len() + 1;
            if actual > self.max_columns {
                return Err(Error::TooManyColumns {
                    actual,
                    maximum: self.max_columns,
                });
            }
            columns.push(ColumnSchema::new(name, data_type));

            match self.current().kind {
                TokenKind::Comma => {
                    self.advance();
                }
                TokenKind::RightParenthesis => {
                    self.advance();
                    break;
                }
                _ => return self.syntax("expected ',' or ')' after column definition"),
            }
        }

        self.finish_statement()?;
        Ok(CreateTable { name, columns })
    }

    fn parse_insert_body(&mut self) -> Result<Insert> {
        self.expect_keyword("INSERT")?;
        self.expect_keyword("INTO")?;
        let table_name = self.expect_identifier("a table name")?.0;
        self.expect_keyword("VALUES")?;

        let mut rows = Vec::new();
        loop {
            self.expect_symbol(TokenKind::LeftParenthesis, "'('")?;
            let mut row = Vec::new();
            if !matches!(self.current().kind, TokenKind::RightParenthesis) {
                loop {
                    row.push(self.parse_value()?);
                    if matches!(self.current().kind, TokenKind::Comma) {
                        self.advance();
                    } else {
                        break;
                    }
                }
            }
            self.expect_symbol(TokenKind::RightParenthesis, "')'")?;

            rows.push(row);
            if rows.len() > self.max_batch_rows {
                return Err(InsertError::BatchTooLarge {
                    actual: rows.len(),
                    maximum: self.max_batch_rows,
                }
                .into());
            }

            if matches!(self.current().kind, TokenKind::Comma) {
                self.advance();
            } else {
                break;
            }
        }

        self.finish_statement()?;
        Ok(Insert { table_name, rows })
    }

    fn parse_value(&mut self) -> Result<Value> {
        let token = self.current().clone();
        let value = match token.kind {
            TokenKind::Number(value) if value.contains(['.', 'e', 'E']) => {
                let parsed = value.parse::<f64>().map_err(|_| Error::Syntax {
                    position: token.position,
                    message: format!("invalid Float64 literal {value:?}"),
                })?;
                Value::Float64(parsed)
            }
            TokenKind::Number(value) => {
                let parsed = value.parse::<i64>().map_err(|_| Error::Syntax {
                    position: token.position,
                    message: format!("Int64 literal {value:?} is out of range"),
                })?;
                Value::Int64(parsed)
            }
            TokenKind::String(value) => Value::String(value),
            TokenKind::Identifier(value) if value.eq_ignore_ascii_case("TRUE") => Value::Bool(true),
            TokenKind::Identifier(value) if value.eq_ignore_ascii_case("FALSE") => {
                Value::Bool(false)
            }
            _ => return self.syntax("expected an integer, float, boolean, or string literal"),
        };
        self.advance();
        Ok(value)
    }

    fn finish_statement(&mut self) -> Result<()> {
        if matches!(self.current().kind, TokenKind::Semicolon) {
            self.advance();
        }
        if !matches!(self.current().kind, TokenKind::End) {
            return self.syntax("expected the end of the statement");
        }
        Ok(())
    }

    fn current_is_keyword(&self, expected: &str) -> bool {
        matches!(&self.current().kind, TokenKind::Identifier(value) if value.eq_ignore_ascii_case(expected))
    }

    fn expect_keyword(&mut self, expected: &str) -> Result<()> {
        if self.current_is_keyword(expected) {
            self.advance();
            Ok(())
        } else {
            self.syntax(format!("expected keyword {expected}"))
        }
    }

    fn expect_identifier(&mut self, expected: &str) -> Result<(String, usize)> {
        let token = self.current();
        if let TokenKind::Identifier(value) = &token.kind {
            let result = (value.clone(), token.position);
            self.advance();
            Ok(result)
        } else {
            self.syntax(format!("expected {expected}"))
        }
    }

    fn expect_symbol(&mut self, expected: TokenKind, description: &str) -> Result<()> {
        if self.current().kind == expected {
            self.advance();
            Ok(())
        } else {
            self.syntax(format!("expected {description}"))
        }
    }

    fn current(&self) -> &Token {
        &self.tokens[self.position]
    }

    fn advance(&mut self) {
        self.position += 1;
    }

    fn syntax<T>(&self, message: impl Into<String>) -> Result<T> {
        Err(Error::Syntax {
            position: self.current().position,
            message: message.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_positions_are_byte_offsets() {
        let error = parse_create_table("CREATE TABLE t (name String) é", 10)
            .expect_err("non-ASCII identifier is unsupported");
        assert_eq!(
            error,
            Error::Syntax {
                position: 29,
                message: "unexpected character '\u{e9}'".to_owned(),
            }
        );
    }

    #[test]
    fn parses_all_insert_literal_types_and_sql_string_escaping() {
        let statement = parse_statement(
            "INSERT INTO Events VALUES (-1, +2.5, TRUE, 'O''Brien'), (+3, -4e-2, false, '')",
            10,
            10,
        )
        .expect("valid INSERT");

        let Statement::Insert(insert) = statement else {
            panic!("expected INSERT");
        };
        assert_eq!(insert.table_name(), "Events");
        assert_eq!(
            insert.rows(),
            [
                vec![
                    Value::Int64(-1),
                    Value::Float64(2.5),
                    Value::Bool(true),
                    Value::String("O'Brien".to_owned()),
                ],
                vec![
                    Value::Int64(3),
                    Value::Float64(-0.04),
                    Value::Bool(false),
                    Value::String(String::new()),
                ],
            ]
        );
    }

    #[test]
    fn enforces_the_insert_row_bound_while_parsing() {
        assert_eq!(
            parse_statement("INSERT INTO t VALUES (1), (2), (3)", 10, 2),
            Err(Error::Insert(InsertError::BatchTooLarge {
                actual: 3,
                maximum: 2,
            }))
        );
    }
}
