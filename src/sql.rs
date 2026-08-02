//! Tokenization and parsing for the supported SQL subset.

use crate::{ColumnSchema, DataType, Error, Result};

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

/// Parse exactly one `CREATE TABLE` statement.
///
/// The byte-size bound is enforced by [`crate::Database`]; this function
/// enforces the caller-provided column bound while parsing.
pub fn parse_create_table(input: &str, max_columns: usize) -> Result<CreateTable> {
    let tokens = Lexer::new(input).tokenize()?;
    Parser::new(tokens, max_columns).parse_create_table()
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Token {
    kind: TokenKind,
    position: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum TokenKind {
    Identifier(String),
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

    fn parse_create_table(mut self) -> Result<CreateTable> {
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

        if matches!(self.current().kind, TokenKind::Semicolon) {
            self.advance();
        }
        if !matches!(self.current().kind, TokenKind::End) {
            return self.syntax("expected the end of the statement");
        }

        Ok(CreateTable { name, columns })
    }

    fn expect_keyword(&mut self, expected: &str) -> Result<()> {
        let token = self.current();
        if matches!(&token.kind, TokenKind::Identifier(value) if value.eq_ignore_ascii_case(expected))
        {
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
}
