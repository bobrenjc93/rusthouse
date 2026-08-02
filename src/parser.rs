use crate::{QueryError, Value};

pub(crate) struct Selection {
    pub(crate) identifier: String,
    pub(crate) value: Value,
}

pub(crate) fn parse(sql: &str) -> Result<Selection, QueryError> {
    let mut parser = Parser::new(sql);
    parser.skip_whitespace();
    if parser.is_at_end() {
        return Err(QueryError::EmptyQuery);
    }

    parser.expect_keyword("SELECT")?;
    parser.require_whitespace("expected a literal after SELECT")?;
    let value = parser.parse_literal()?;
    parser.require_whitespace("expected AS after the literal")?;
    parser.expect_keyword("AS")?;
    parser.require_whitespace("expected an identifier after AS")?;
    let identifier = parser.parse_identifier()?;

    parser.skip_whitespace();
    if parser.consume_byte(b';') {
        parser.skip_whitespace();
    }
    if !parser.is_at_end() {
        return Err(parser.error("expected the end of the statement"));
    }

    Ok(Selection { identifier, value })
}

struct Parser<'a> {
    input: &'a str,
    position: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, position: 0 }
    }

    fn remaining(&self) -> &'a str {
        &self.input[self.position..]
    }

    fn is_at_end(&self) -> bool {
        self.position == self.input.len()
    }

    fn error(&self, message: impl Into<String>) -> QueryError {
        QueryError::syntax(self.position, message)
    }

    fn skip_whitespace(&mut self) -> bool {
        let start = self.position;
        while let Some(character) = self.remaining().chars().next() {
            if !character.is_whitespace() {
                break;
            }
            self.position += character.len_utf8();
        }
        self.position != start
    }

    fn require_whitespace(&mut self, message: &'static str) -> Result<(), QueryError> {
        if self.skip_whitespace() {
            Ok(())
        } else {
            Err(self.error(message))
        }
    }

    fn consume_byte(&mut self, expected: u8) -> bool {
        if self.input.as_bytes().get(self.position) == Some(&expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn expect_keyword(&mut self, expected: &'static str) -> Result<(), QueryError> {
        let remaining = self.remaining();
        let Some(candidate) = remaining.get(..expected.len()) else {
            return Err(self.error(format!("expected {expected}")));
        };
        if !candidate.eq_ignore_ascii_case(expected) {
            return Err(self.error(format!("expected {expected}")));
        }

        if remaining[expected.len()..]
            .chars()
            .next()
            .is_some_and(is_identifier_continue)
        {
            return Err(self.error(format!("expected {expected}")));
        }

        self.position += expected.len();
        Ok(())
    }

    fn parse_literal(&mut self) -> Result<Value, QueryError> {
        match self.remaining().as_bytes().first().copied() {
            Some(b'\'') => self.parse_string().map(Value::String),
            Some(b'+') | Some(b'-') | Some(b'.') | Some(b'0'..=b'9') => self.parse_number(),
            Some(_) if self.starts_with_keyword("TRUE") => {
                self.position += "TRUE".len();
                Ok(Value::Bool(true))
            }
            Some(_) if self.starts_with_keyword("FALSE") => {
                self.position += "FALSE".len();
                Ok(Value::Bool(false))
            }
            Some(_) => Err(self.error("expected an integer, float, boolean, or quoted string")),
            None => Err(self.error("expected a literal")),
        }
    }

    fn starts_with_keyword(&self, expected: &str) -> bool {
        let remaining = self.remaining();
        let Some(candidate) = remaining.get(..expected.len()) else {
            return false;
        };
        candidate.eq_ignore_ascii_case(expected)
            && !remaining[expected.len()..]
                .chars()
                .next()
                .is_some_and(is_identifier_continue)
    }

    fn parse_string(&mut self) -> Result<String, QueryError> {
        debug_assert!(self.consume_byte(b'\''));
        let mut value = String::new();

        loop {
            let Some(character) = self.remaining().chars().next() else {
                return Err(self.error("unterminated string literal"));
            };
            self.position += character.len_utf8();
            if character != '\'' {
                value.push(character);
                continue;
            }
            if self.consume_byte(b'\'') {
                value.push('\'');
            } else {
                return Ok(value);
            }
        }
    }

    fn parse_number(&mut self) -> Result<Value, QueryError> {
        let start = self.position;
        if matches!(self.input.as_bytes().get(self.position), Some(b'+' | b'-')) {
            self.position += 1;
        }

        let integer_digits = self.consume_ascii_digits();
        let decimal_digits = if self.consume_byte(b'.') {
            Some(self.consume_ascii_digits())
        } else {
            None
        };

        if integer_digits == 0 && decimal_digits.unwrap_or(0) == 0 {
            return Err(self.error("expected digits in numeric literal"));
        }

        let has_exponent = if matches!(self.input.as_bytes().get(self.position), Some(b'e' | b'E'))
        {
            self.position += 1;
            if matches!(self.input.as_bytes().get(self.position), Some(b'+' | b'-')) {
                self.position += 1;
            }
            if self.consume_ascii_digits() == 0 {
                return Err(self.error("expected digits in float exponent"));
            }
            true
        } else {
            false
        };

        let literal = &self.input[start..self.position];
        if decimal_digits.is_some() || has_exponent {
            let value = literal
                .parse::<f64>()
                .map_err(|_| self.error("invalid float literal"))?;
            if !value.is_finite() {
                return Err(QueryError::NonFiniteFloat {
                    literal: literal.to_owned(),
                });
            }
            Ok(Value::Float64(value))
        } else {
            literal
                .parse::<i64>()
                .map(Value::Int64)
                .map_err(|_| QueryError::IntegerOutOfRange {
                    literal: literal.to_owned(),
                })
        }
    }

    fn consume_ascii_digits(&mut self) -> usize {
        let start = self.position;
        while matches!(self.input.as_bytes().get(self.position), Some(b'0'..=b'9')) {
            self.position += 1;
        }
        self.position - start
    }

    fn parse_identifier(&mut self) -> Result<String, QueryError> {
        if self.consume_byte(b'"') {
            return self.parse_quoted_identifier();
        }

        let start = self.position;
        let Some(first) = self.remaining().chars().next() else {
            return Err(self.error("expected an identifier"));
        };
        if !is_identifier_start(first) {
            return Err(self.error("identifier must start with a letter or underscore"));
        }
        self.position += first.len_utf8();
        while let Some(character) = self.remaining().chars().next() {
            if !is_identifier_continue(character) {
                break;
            }
            self.position += character.len_utf8();
        }
        Ok(self.input[start..self.position].to_owned())
    }

    fn parse_quoted_identifier(&mut self) -> Result<String, QueryError> {
        let mut identifier = String::new();
        loop {
            let Some(character) = self.remaining().chars().next() else {
                return Err(self.error("unterminated quoted identifier"));
            };
            self.position += character.len_utf8();
            if character != '"' {
                identifier.push(character);
                continue;
            }
            if self.consume_byte(b'"') {
                identifier.push('"');
            } else if identifier.is_empty() {
                return Err(self.error("identifier cannot be empty"));
            } else {
                return Ok(identifier);
            }
        }
    }
}

fn is_identifier_start(character: char) -> bool {
    character == '_' || character.is_alphabetic()
}

fn is_identifier_continue(character: char) -> bool {
    character == '_' || character.is_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_finite_float() {
        assert!(matches!(
            parse("SELECT 1e999 AS huge"),
            Err(QueryError::NonFiniteFloat { .. })
        ));
    }

    #[test]
    fn accepts_statement_whitespace_and_semicolon() {
        let selection = parse(" \nselect\t-1.5e2 as \"a b\"; \r\n").unwrap();
        assert_eq!(selection.identifier, "a b");
        assert_eq!(selection.value, Value::Float64(-150.0));
    }
}
