//! RustHouse is an experimental, compact analytical database.

use std::borrow::Cow;
use std::fmt;
use std::io::{self, Write};

/// Maximum number of SQL bytes accepted from a single CLI invocation.
pub const MAX_SQL_INPUT_BYTES: usize = 1024 * 1024;

/// Returns the product name.
pub fn product_name() -> &'static str {
    "RustHouse"
}

/// A scalar value supported by the initial query surface.
#[derive(Debug, Clone, PartialEq)]
pub enum ScalarValue {
    Integer(i64),
    Float(f64),
    Boolean(bool),
    String(String),
}

impl ScalarValue {
    fn validate_for_csv(&self) -> io::Result<()> {
        if matches!(self, Self::Float(value) if !value.is_finite()) {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot serialize a non-finite float as CSV",
            ))
        } else {
            Ok(())
        }
    }

    fn csv_value(&self) -> Cow<'_, str> {
        match self {
            Self::Integer(value) => Cow::Owned(value.to_string()),
            Self::Float(value) => {
                let mut rendered = value.to_string();
                if !rendered.contains(['.', 'e', 'E']) {
                    rendered.push_str(".0");
                }
                Cow::Owned(rendered)
            }
            Self::Boolean(true) => Cow::Borrowed("true"),
            Self::Boolean(false) => Cow::Borrowed("false"),
            Self::String(value) => Cow::Borrowed(value),
        }
    }
}

/// The single-column result of a scalar `SELECT` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    pub header: String,
    pub value: ScalarValue,
}

/// A syntax or literal validation error in a SQL batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlError {
    line: usize,
    column: usize,
    message: String,
}

impl fmt::Display for SqlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "SQL error at line {}, column {}: {}",
            self.line, self.column, self.message
        )
    }
}

impl std::error::Error for SqlError {}

/// Parses a batch of `SELECT <literal> [AS <identifier>];` statements.
pub fn parse_sql_batch(input: &str) -> Result<Vec<QueryResult>, SqlError> {
    Parser::new(input).parse_batch()
}

/// Writes each query result as a CSV header followed by its single row.
pub fn write_csv<W: Write>(results: &[QueryResult], mut writer: W) -> io::Result<()> {
    for result in results {
        result.value.validate_for_csv()?;
    }

    for result in results {
        write_csv_field(&mut writer, &result.header)?;
        writer.write_all(b"\n")?;

        write_csv_field(&mut writer, &result.value.csv_value())?;
        writer.write_all(b"\n")?;
    }
    Ok(())
}

fn write_csv_field<W: Write>(writer: &mut W, value: &str) -> io::Result<()> {
    let requires_quotes = value.is_empty()
        || value
            .as_bytes()
            .iter()
            .any(|byte| matches!(byte, b',' | b'"' | b'\r' | b'\n'));

    if !requires_quotes {
        return writer.write_all(value.as_bytes());
    }

    writer.write_all(b"\"")?;
    for section in value.split_inclusive('"') {
        writer.write_all(section.as_bytes())?;
        if section.ends_with('"') {
            writer.write_all(b"\"")?;
        }
    }
    writer.write_all(b"\"")
}

struct Parser<'a> {
    input: &'a str,
    position: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, position: 0 }
    }

    fn parse_batch(mut self) -> Result<Vec<QueryResult>, SqlError> {
        let mut results = Vec::new();
        self.skip_whitespace();

        if self.is_at_end() {
            return Err(self.error("expected a SELECT statement"));
        }

        while !self.is_at_end() {
            results.push(self.parse_select()?);
            self.skip_whitespace();
        }

        Ok(results)
    }

    fn parse_select(&mut self) -> Result<QueryResult, SqlError> {
        if !self.consume_keyword("SELECT") {
            return Err(self.error("expected SELECT"));
        }
        self.skip_whitespace();

        let literal_start = self.position;
        let value = self.parse_literal()?;
        let literal_end = self.position;
        self.skip_whitespace();

        let header = if self.consume_keyword("AS") {
            self.skip_whitespace();
            self.parse_identifier()?
        } else {
            self.input[literal_start..literal_end].to_owned()
        };

        self.skip_whitespace();
        if self.peek() != Some(';') {
            return Err(self.error("expected ';' after SELECT statement"));
        }
        self.advance();

        Ok(QueryResult { header, value })
    }

    fn parse_literal(&mut self) -> Result<ScalarValue, SqlError> {
        match self.peek() {
            Some('\'') => self.parse_string().map(ScalarValue::String),
            Some('+') | Some('-') | Some('.') | Some('0'..='9') => self.parse_number(),
            _ if self.consume_keyword("TRUE") => Ok(ScalarValue::Boolean(true)),
            _ if self.consume_keyword("FALSE") => Ok(ScalarValue::Boolean(false)),
            _ => {
                Err(self
                    .error("expected an integer, finite float, boolean, or quoted string literal"))
            }
        }
    }

    fn parse_string(&mut self) -> Result<String, SqlError> {
        let start = self.position;
        self.advance();
        let mut value = String::new();

        loop {
            match self.advance() {
                Some('\'') if self.peek() == Some('\'') => {
                    self.advance();
                    value.push('\'');
                }
                Some('\'') => return Ok(value),
                Some(character) => value.push(character),
                None => return Err(self.error_at(start, "unterminated quoted string")),
            }
        }
    }

    fn parse_number(&mut self) -> Result<ScalarValue, SqlError> {
        let start = self.position;
        if matches!(self.peek(), Some('+') | Some('-')) {
            self.advance();
        }

        let digits_before_decimal = self.consume_digits();
        let has_decimal = if self.peek() == Some('.') {
            self.advance();
            true
        } else {
            false
        };
        let digits_after_decimal = if has_decimal {
            self.consume_digits()
        } else {
            0
        };

        if digits_before_decimal + digits_after_decimal == 0 {
            return Err(self.error_at(start, "invalid numeric literal"));
        }

        let has_exponent = if matches!(self.peek(), Some('e') | Some('E')) {
            let exponent_start = self.position;
            self.advance();
            if matches!(self.peek(), Some('+') | Some('-')) {
                self.advance();
            }
            if self.consume_digits() == 0 {
                return Err(self.error_at(exponent_start, "invalid float exponent"));
            }
            true
        } else {
            false
        };

        let literal = &self.input[start..self.position];
        if has_decimal || has_exponent {
            let value = literal
                .parse::<f64>()
                .map_err(|_| self.error_at(start, "invalid float literal"))?;
            if !value.is_finite() {
                return Err(self.error_at(start, "float literal must be finite"));
            }
            Ok(ScalarValue::Float(value))
        } else {
            literal
                .parse::<i64>()
                .map(ScalarValue::Integer)
                .map_err(|_| self.error_at(start, "integer literal is outside the Int64 range"))
        }
    }

    fn parse_identifier(&mut self) -> Result<String, SqlError> {
        let start = self.position;
        match self.peek() {
            Some(character) if character.is_ascii_alphabetic() || character == '_' => {
                self.advance();
            }
            _ => return Err(self.error("expected an identifier after AS")),
        }

        while matches!(self.peek(), Some(character) if character.is_ascii_alphanumeric() || character == '_')
        {
            self.advance();
        }

        Ok(self.input[start..self.position].to_owned())
    }

    fn consume_digits(&mut self) -> usize {
        let mut count = 0;
        while matches!(self.peek(), Some('0'..='9')) {
            self.advance();
            count += 1;
        }
        count
    }

    fn consume_keyword(&mut self, keyword: &str) -> bool {
        let Some(candidate) = self
            .input
            .get(self.position..self.position.saturating_add(keyword.len()))
        else {
            return false;
        };
        if !candidate.eq_ignore_ascii_case(keyword) {
            return false;
        }

        let end = self.position + keyword.len();
        if matches!(self.input[end..].chars().next(), Some(character) if character.is_ascii_alphanumeric() || character == '_')
        {
            return false;
        }

        self.position = end;
        true
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(character) if character.is_whitespace()) {
            self.advance();
        }
    }

    fn peek(&self) -> Option<char> {
        self.input[self.position..].chars().next()
    }

    fn advance(&mut self) -> Option<char> {
        let character = self.peek()?;
        self.position += character.len_utf8();
        Some(character)
    }

    fn is_at_end(&self) -> bool {
        self.position == self.input.len()
    }

    fn error(&self, message: impl Into<String>) -> SqlError {
        self.error_at(self.position, message)
    }

    fn error_at(&self, position: usize, message: impl Into<String>) -> SqlError {
        let (line, column) = line_and_column(self.input, position);
        SqlError {
            line,
            column,
            message: message.into(),
        }
    }
}

fn line_and_column(input: &str, position: usize) -> (usize, usize) {
    let mut line = 1;
    let mut column = 1;
    for character in input[..position].chars() {
        if character == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (line, column)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_the_database() {
        assert_eq!(product_name(), "RustHouse");
    }

    #[test]
    fn parses_supported_literals_and_aliases() {
        let results = parse_sql_batch(
            "SELECT -12 AS integer_value; SELECT +.5 AS float_value; \
             SELECT FALSE; SELECT 'it''s text' AS string_value;",
        )
        .unwrap();

        assert_eq!(results[0].value, ScalarValue::Integer(-12));
        assert_eq!(results[1].value, ScalarValue::Float(0.5));
        assert_eq!(results[2].header, "FALSE");
        assert_eq!(results[2].value, ScalarValue::Boolean(false));
        assert_eq!(
            results[3].value,
            ScalarValue::String("it's text".to_owned())
        );
    }

    #[test]
    fn quotes_csv_fields() {
        let results = vec![QueryResult {
            header: "message, \"text\"".to_owned(),
            value: ScalarValue::String("one, \"two\"\nthree".to_owned()),
        }];
        let mut output = Vec::new();

        write_csv(&results, &mut output).unwrap();

        assert_eq!(
            String::from_utf8(output).unwrap(),
            "\"message, \"\"text\"\"\"\n\"one, \"\"two\"\"\nthree\"\n"
        );
    }

    #[test]
    fn rejects_non_finite_float_values_before_writing() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let results = vec![QueryResult {
                header: "value".to_owned(),
                value: ScalarValue::Float(value),
            }];
            let mut output = Vec::new();

            let error = write_csv(&results, &mut output).unwrap_err();

            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert!(error.to_string().contains("non-finite float"));
            assert!(output.is_empty());
        }
    }
}
