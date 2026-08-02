//! Tokenization and parsing for the supported SQL subset.

use std::collections::HashSet;

use crate::{ColumnSchema, DataType, Error, Result, Value};

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

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Insert {
    pub(crate) table: String,
    pub(crate) rows: Vec<Vec<Value>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Projection {
    All,
    Columns(Vec<String>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SortDirection {
    Ascending,
    Descending,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OrderBy {
    pub(crate) column: String,
    pub(crate) direction: SortDirection,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Select {
    pub(crate) table: String,
    pub(crate) projection: Projection,
    pub(crate) order_by: Vec<OrderBy>,
    pub(crate) limit: Option<usize>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Statement {
    CreateTable(CreateTable),
    Insert(Insert),
    Select(Select),
}

/// Parse exactly one `CREATE TABLE` statement.
///
/// The byte-size bound is enforced by [`crate::Database`]; this function
/// enforces the caller-provided column bound while parsing.
pub fn parse_create_table(input: &str, max_columns: usize) -> Result<CreateTable> {
    match parse_one(input, max_columns)? {
        Statement::CreateTable(statement) => Ok(statement),
        Statement::Insert(_) | Statement::Select(_) => Err(Error::Syntax {
            position: 0,
            message: "expected keyword CREATE".to_owned(),
        }),
    }
}

pub(crate) fn parse_one(input: &str, max_columns: usize) -> Result<Statement> {
    let tokens = Lexer::new(input).tokenize()?;
    Parser::new(tokens, max_columns).parse_one()
}

pub(crate) fn parse_batch(input: &str, max_columns: usize) -> Result<Vec<Statement>> {
    let tokens = Lexer::new(input).tokenize()?;
    Parser::new(tokens, max_columns).parse_batch()
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
    Asterisk,
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
                '*' => {
                    self.advance();
                    TokenKind::Asterisk
                }
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
                character
                    if character.is_ascii_digit()
                        || (character == '-'
                            && self.next().is_some_and(|next| next.is_ascii_digit())) =>
                {
                    TokenKind::Number(self.scan_number())
                }
                character if character.is_ascii_alphabetic() || character == '_' => {
                    TokenKind::Identifier(self.scan_identifier())
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

    fn scan_number(&mut self) -> String {
        let start = self.position;
        if self.current() == Some('-') {
            self.advance();
        }
        while self
            .current()
            .is_some_and(|character| character.is_ascii_digit())
        {
            self.advance();
        }
        if self.current() == Some('.') {
            self.advance();
            while self
                .current()
                .is_some_and(|character| character.is_ascii_digit())
            {
                self.advance();
            }
        }
        if self
            .current()
            .is_some_and(|character| matches!(character, 'e' | 'E'))
        {
            self.advance();
            if self
                .current()
                .is_some_and(|character| matches!(character, '+' | '-'))
            {
                self.advance();
            }
            while self
                .current()
                .is_some_and(|character| character.is_ascii_digit())
            {
                self.advance();
            }
        }
        self.input[start..self.position].to_owned()
    }

    fn scan_string(&mut self, start: usize) -> Result<String> {
        self.advance();
        let mut value = String::new();
        loop {
            match self.current() {
                None => {
                    return Err(Error::Syntax {
                        position: start,
                        message: "unterminated string literal".to_owned(),
                    });
                }
                Some('\'') if self.next() == Some('\'') => {
                    value.push('\'');
                    self.advance();
                    self.advance();
                }
                Some('\'') => {
                    self.advance();
                    return Ok(value);
                }
                Some(character) => {
                    value.push(character);
                    self.advance();
                }
            }
        }
    }
}

struct Parser {
    tokens: Vec<Token>,
    position: usize,
    max_columns: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>, max_columns: usize) -> Self {
        Self {
            tokens,
            position: 0,
            max_columns,
        }
    }

    fn parse_one(mut self) -> Result<Statement> {
        if matches!(self.current().kind, TokenKind::End) {
            return self.syntax("expected a SQL statement");
        }
        let statement = self.parse_statement()?;
        if matches!(self.current().kind, TokenKind::Semicolon) {
            self.advance();
        }
        if !matches!(self.current().kind, TokenKind::End) {
            return self.syntax("expected the end of the statement");
        }
        Ok(statement)
    }

    fn parse_batch(mut self) -> Result<Vec<Statement>> {
        if matches!(self.current().kind, TokenKind::End) {
            return self.syntax("expected a SQL statement");
        }

        let mut statements = Vec::new();
        while !matches!(self.current().kind, TokenKind::End) {
            statements.push(self.parse_statement()?);
            match self.current().kind {
                TokenKind::Semicolon => self.advance(),
                TokenKind::End => break,
                _ => return self.syntax("expected ';' between statements"),
            }
        }
        Ok(statements)
    }

    fn parse_statement(&mut self) -> Result<Statement> {
        if self.current_is_keyword("CREATE") {
            self.parse_create_table().map(Statement::CreateTable)
        } else if self.current_is_keyword("INSERT") {
            self.parse_insert().map(Statement::Insert)
        } else if self.current_is_keyword("SELECT") {
            self.parse_select().map(Statement::Select)
        } else {
            self.syntax("expected CREATE, INSERT, or SELECT")
        }
    }

    fn parse_create_table(&mut self) -> Result<CreateTable> {
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
                TokenKind::Comma => self.advance(),
                TokenKind::RightParenthesis => {
                    self.advance();
                    break;
                }
                _ => return self.syntax("expected ',' or ')' after column definition"),
            }
        }

        Ok(CreateTable { name, columns })
    }

    fn parse_insert(&mut self) -> Result<Insert> {
        self.expect_keyword("INSERT")?;
        self.expect_keyword("INTO")?;
        let table = self.expect_identifier("a table name")?.0;
        self.expect_keyword("VALUES")?;

        let mut rows = Vec::new();
        loop {
            self.expect_symbol(TokenKind::LeftParenthesis, "'('")?;
            let mut row = Vec::new();
            if matches!(self.current().kind, TokenKind::RightParenthesis) {
                self.advance();
            } else {
                loop {
                    row.push(self.parse_literal()?);
                    match self.current().kind {
                        TokenKind::Comma => self.advance(),
                        TokenKind::RightParenthesis => {
                            self.advance();
                            break;
                        }
                        _ => return self.syntax("expected ',' or ')' after value"),
                    }
                }
            }
            rows.push(row);

            if matches!(self.current().kind, TokenKind::Comma) {
                self.advance();
            } else {
                break;
            }
        }

        Ok(Insert { table, rows })
    }

    fn parse_literal(&mut self) -> Result<Value> {
        let token = self.current().clone();
        match token.kind {
            TokenKind::String(value) => {
                self.advance();
                Ok(Value::String(value))
            }
            TokenKind::Identifier(value) if value.eq_ignore_ascii_case("TRUE") => {
                self.advance();
                Ok(Value::Bool(true))
            }
            TokenKind::Identifier(value) if value.eq_ignore_ascii_case("FALSE") => {
                self.advance();
                Ok(Value::Bool(false))
            }
            TokenKind::Number(value)
                if value.contains('.') || value.contains('e') || value.contains('E') =>
            {
                self.advance();
                value
                    .parse::<f64>()
                    .map(Value::Float64)
                    .map_err(|_| Error::InvalidLiteral {
                        value,
                        position: token.position,
                        expected: "Float64",
                    })
            }
            TokenKind::Number(value) => {
                self.advance();
                value
                    .parse::<i64>()
                    .map(Value::Int64)
                    .map_err(|_| Error::InvalidLiteral {
                        value,
                        position: token.position,
                        expected: "Int64",
                    })
            }
            _ => self.syntax("expected a String, Int64, Float64, or Bool literal"),
        }
    }

    fn parse_select(&mut self) -> Result<Select> {
        self.expect_keyword("SELECT")?;
        let projection = if matches!(self.current().kind, TokenKind::Asterisk) {
            self.advance();
            Projection::All
        } else {
            let mut columns = Vec::new();
            loop {
                columns.push(self.expect_identifier("a projected column or '*'")?.0);
                if matches!(self.current().kind, TokenKind::Comma) {
                    self.advance();
                } else {
                    break;
                }
            }
            Projection::Columns(columns)
        };
        self.expect_keyword("FROM")?;
        let table = self.expect_identifier("a table name")?.0;

        let mut order_by = Vec::new();
        let mut ordered_columns = HashSet::new();
        if self.current_is_keyword("ORDER") {
            self.advance();
            self.expect_keyword("BY")?;
            loop {
                let column = self.expect_identifier("an ORDER BY column")?.0;
                let direction = if self.current_is_keyword("ASC") {
                    self.advance();
                    SortDirection::Ascending
                } else if self.current_is_keyword("DESC") {
                    self.advance();
                    SortDirection::Descending
                } else {
                    SortDirection::Ascending
                };
                if ordered_columns.insert(column.to_ascii_lowercase()) {
                    let actual = order_by.len() + 1;
                    if actual > self.max_columns {
                        return Err(Error::TooManyOrderByColumns {
                            actual,
                            maximum: self.max_columns,
                        });
                    }
                    order_by.push(OrderBy { column, direction });
                }

                if matches!(self.current().kind, TokenKind::Comma) {
                    self.advance();
                } else {
                    break;
                }
            }
        }

        let limit = if self.current_is_keyword("LIMIT") {
            self.advance();
            let token = self.current().clone();
            let TokenKind::Number(value) = token.kind else {
                return self.syntax("expected a nonnegative integer after LIMIT");
            };
            self.advance();
            Some(value.parse::<usize>().map_err(|_| Error::InvalidLiteral {
                value,
                position: token.position,
                expected: "nonnegative LIMIT",
            })?)
        } else {
            None
        };

        Ok(Select {
            table,
            projection,
            order_by,
            limit,
        })
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
    fn parses_a_complete_statement_batch_and_unescapes_strings() {
        let statements = parse_batch(
            "CREATE TABLE t (id Int64, label String);\n\
             INSERT INTO t VALUES (1, 'it''s one'), (2, 'two');\n\
             SELECT label, id FROM t;",
            10,
        )
        .expect("valid batch");

        assert_eq!(statements.len(), 3);
        assert_eq!(
            statements[1],
            Statement::Insert(Insert {
                table: "t".to_owned(),
                rows: vec![
                    vec![Value::Int64(1), Value::String("it's one".to_owned())],
                    vec![Value::Int64(2), Value::String("two".to_owned())],
                ],
            })
        );
        assert_eq!(
            statements[2],
            Statement::Select(Select {
                table: "t".to_owned(),
                projection: Projection::Columns(vec!["label".to_owned(), "id".to_owned()]),
                order_by: Vec::new(),
                limit: None,
            })
        );
    }

    #[test]
    fn parses_ordering_directions_and_limit() {
        let statement = parse_one(
            "SELECT id FROM events ORDER BY active DESC, score, label ASC LIMIT 0",
            10,
        )
        .expect("ordered limited SELECT");

        assert_eq!(
            statement,
            Statement::Select(Select {
                table: "events".to_owned(),
                projection: Projection::Columns(vec!["id".to_owned()]),
                order_by: vec![
                    OrderBy {
                        column: "active".to_owned(),
                        direction: SortDirection::Descending,
                    },
                    OrderBy {
                        column: "score".to_owned(),
                        direction: SortDirection::Ascending,
                    },
                    OrderBy {
                        column: "label".to_owned(),
                        direction: SortDirection::Ascending,
                    },
                ],
                limit: Some(0),
            })
        );
    }

    #[test]
    fn rejects_invalid_limits() {
        for limit in ["-1", "1.5", "1e2", "184467440737095516160"] {
            let input = format!("SELECT * FROM events LIMIT {limit}");
            assert!(parse_one(&input, 10).is_err(), "accepted LIMIT {limit}");
        }
    }

    #[test]
    fn deduplicates_and_bounds_order_by_columns() {
        let statement = parse_one("SELECT * FROM events ORDER BY Key DESC, key ASC, other", 2)
            .expect("duplicate ORDER BY columns do not consume the bound");
        let Statement::Select(select) = statement else {
            panic!("expected SELECT");
        };
        assert_eq!(
            select.order_by,
            [
                OrderBy {
                    column: "Key".to_owned(),
                    direction: SortDirection::Descending,
                },
                OrderBy {
                    column: "other".to_owned(),
                    direction: SortDirection::Ascending,
                },
            ]
        );

        assert_eq!(
            parse_one("SELECT * FROM events ORDER BY a, b, c", 2),
            Err(Error::TooManyOrderByColumns {
                actual: 3,
                maximum: 2,
            })
        );
    }
}
