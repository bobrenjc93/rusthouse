use super::limits::enforce_sql_size;
use super::{InsertParseLimits, InsertStatement, ParseError, Value};

/// Parses one bounded `INSERT INTO ... VALUES` statement using the default
/// limits.
///
/// Integer literals become [`Value::Int64`], while literals containing a
/// decimal point or exponent become [`Value::Float64`]. Boolean keywords are
/// ASCII case-insensitive. String literals use single quotes and escape a
/// quote by doubling it.
///
/// # Errors
///
/// Returns [`ParseError`] when the input exceeds the default resource limits,
/// contains an invalid literal, or does not match the supported `INSERT`
/// grammar.
///
/// # Examples
///
/// ```
/// use rusthouse::sql::{Value, parse_insert};
///
/// let statement = parse_insert(
///     "INSERT INTO readings VALUES (-2, 1.5, TRUE, 'it''s ready'), (3, -4e-1, false, 'next');",
/// )?;
/// assert_eq!(statement.table_name, "readings");
/// assert_eq!(statement.rows.len(), 2);
/// assert_eq!(statement.rows[0][0], Value::Int64(-2));
/// assert_eq!(statement.rows[0][3], Value::String("it's ready".into()));
/// # Ok::<(), rusthouse::sql::ParseError>(())
/// ```
pub fn parse_insert(sql: &str) -> Result<InsertStatement, ParseError> {
    parse_insert_with_limits(sql, InsertParseLimits::default())
}

/// Parses one `INSERT INTO ... VALUES` statement using caller-provided
/// resource limits.
///
/// # Errors
///
/// Returns [`ParseError`] when the input exceeds `limits`, contains an invalid
/// literal, or does not match the supported `INSERT` grammar.
pub fn parse_insert_with_limits(
    sql: &str,
    limits: InsertParseLimits,
) -> Result<InsertStatement, ParseError> {
    enforce_sql_size(sql, limits.max_sql_bytes)?;
    InsertParser::new(sql, limits).parse()
}

struct InsertParser<'a> {
    sql: &'a str,
    position: usize,
    limits: InsertParseLimits,
    value_count: usize,
    string_bytes: usize,
}

impl<'a> InsertParser<'a> {
    fn new(sql: &'a str, limits: InsertParseLimits) -> Self {
        Self {
            sql,
            position: 0,
            limits,
            value_count: 0,
            string_bytes: 0,
        }
    }

    fn parse(mut self) -> Result<InsertStatement, ParseError> {
        self.expect_keyword("INSERT")?;
        self.expect_keyword("INTO")?;
        let table_name = self.expect_identifier("table name")?;
        self.expect_keyword("VALUES")?;

        let mut rows = Vec::with_capacity(self.limits.max_rows.min(16));
        loop {
            self.skip_whitespace();
            let row_position = self.position;
            self.expect_byte(b'(', "'('")?;
            if rows.len() == self.limits.max_rows {
                return Err(ParseError::TooManyRows {
                    position: row_position,
                    max_rows: self.limits.max_rows,
                });
            }

            let mut row = Vec::new();
            loop {
                self.skip_whitespace();
                let value_position = self.position;
                if self.value_count == self.limits.max_values {
                    return Err(ParseError::TooManyValues {
                        position: value_position,
                        max_values: self.limits.max_values,
                    });
                }

                row.push(self.parse_value()?);
                self.value_count += 1;

                self.skip_whitespace();
                match self.current_byte() {
                    Some(b',') => self.position += 1,
                    Some(b')') => {
                        self.position += 1;
                        break;
                    }
                    _ => return Err(self.syntax_error("',' or ')'")),
                }
            }
            rows.push(row);

            self.skip_whitespace();
            if self.current_byte() == Some(b',') {
                self.position += 1;
            } else {
                break;
            }
        }

        self.finish_statement()?;
        Ok(InsertStatement { table_name, rows })
    }

    fn parse_value(&mut self) -> Result<Value, ParseError> {
        let start = self.position;
        match self.current_byte() {
            Some(b'\'') => self.parse_string(),
            Some(byte) if is_identifier_start(byte) => {
                let end = self.scan_identifier();
                let literal = &self.sql[start..end];
                if literal.eq_ignore_ascii_case("true") {
                    Ok(Value::Bool(true))
                } else if literal.eq_ignore_ascii_case("false") {
                    Ok(Value::Bool(false))
                } else if is_non_finite_name(literal) {
                    Err(ParseError::NonFiniteFloat {
                        position: start,
                        literal: literal.to_owned(),
                    })
                } else {
                    Err(self.syntax_error_at("literal", start, Some(end)))
                }
            }
            Some(b'+' | b'-' | b'.' | b'0'..=b'9') => self.parse_number(),
            _ => Err(self.syntax_error("literal")),
        }
    }

    fn parse_number(&mut self) -> Result<Value, ParseError> {
        let start = self.position;
        while let Some(byte) = self.current_byte() {
            if is_value_delimiter(byte) {
                break;
            }
            self.position += 1;
        }

        let literal = &self.sql[start..self.position];
        let unsigned = literal.strip_prefix(['+', '-']).unwrap_or(literal);
        if is_non_finite_name(unsigned) {
            return Err(ParseError::NonFiniteFloat {
                position: start,
                literal: literal.to_owned(),
            });
        }

        match classify_number(literal) {
            Some(NumberKind::Integer) => {
                literal
                    .parse::<i64>()
                    .map(Value::Int64)
                    .map_err(|_| ParseError::IntegerOverflow {
                        position: start,
                        literal: literal.to_owned(),
                    })
            }
            Some(NumberKind::Float) => match literal.parse::<f64>() {
                Ok(value) if value.is_finite() => Ok(Value::Float64(value)),
                Ok(_) | Err(_) => Err(ParseError::NonFiniteFloat {
                    position: start,
                    literal: literal.to_owned(),
                }),
            },
            None => Err(self.syntax_error_at("literal", start, Some(self.position))),
        }
    }

    fn parse_string(&mut self) -> Result<Value, ParseError> {
        let start = self.position;
        let content_start = start + 1;
        let mut cursor = content_start;
        let mut segment_start = content_start;
        let mut decoded_bytes = 0usize;

        let closing_quote = loop {
            let Some(relative_quote) = self.sql[cursor..].find('\'') else {
                return Err(ParseError::Syntax {
                    position: self.sql.len(),
                    expected: "closing quote",
                    found: None,
                });
            };
            let quote = cursor + relative_quote;
            if self.sql.as_bytes().get(quote + 1) == Some(&b'\'') {
                decoded_bytes = decoded_bytes
                    .checked_add(quote - segment_start)
                    .and_then(|bytes| bytes.checked_add(1))
                    .unwrap_or(usize::MAX);
                cursor = quote + 2;
                segment_start = cursor;
            } else {
                decoded_bytes = decoded_bytes
                    .checked_add(quote - segment_start)
                    .unwrap_or(usize::MAX);
                break quote;
            }
        };

        let attempted_bytes = self
            .string_bytes
            .checked_add(decoded_bytes)
            .unwrap_or(usize::MAX);
        if attempted_bytes > self.limits.max_string_bytes {
            return Err(ParseError::StringByteLimitExceeded {
                position: start,
                max_bytes: self.limits.max_string_bytes,
                attempted_bytes,
            });
        }

        let value = self.sql[content_start..closing_quote].replace("''", "'");
        debug_assert_eq!(value.len(), decoded_bytes);
        self.position = closing_quote + 1;
        self.string_bytes = attempted_bytes;
        Ok(Value::String(value))
    }

    fn expect_keyword(&mut self, expected: &'static str) -> Result<(), ParseError> {
        self.skip_whitespace();
        let start = self.position;
        let Some(byte) = self.current_byte() else {
            return Err(self.syntax_error(expected));
        };
        if !is_identifier_start(byte) {
            return Err(self.syntax_error(expected));
        }

        let end = self.scan_identifier();
        if self.sql[start..end].eq_ignore_ascii_case(expected) {
            Ok(())
        } else {
            Err(self.syntax_error_at(expected, start, Some(end)))
        }
    }

    fn expect_identifier(&mut self, expected: &'static str) -> Result<String, ParseError> {
        self.skip_whitespace();
        let start = self.position;
        match self.current_byte() {
            Some(byte) if is_identifier_start(byte) => {
                let end = self.scan_identifier();
                Ok(self.sql[start..end].to_owned())
            }
            _ => Err(self.syntax_error(expected)),
        }
    }

    fn expect_byte(&mut self, expected_byte: u8, expected: &'static str) -> Result<(), ParseError> {
        if self.current_byte() == Some(expected_byte) {
            self.position += 1;
            Ok(())
        } else {
            Err(self.syntax_error(expected))
        }
    }

    fn finish_statement(&mut self) -> Result<(), ParseError> {
        self.skip_whitespace();
        if self.current_byte() == Some(b';') {
            self.position += 1;
            self.skip_whitespace();
        }
        if self.position != self.sql.len() {
            return Err(ParseError::TrailingInput {
                position: self.position,
            });
        }

        Ok(())
    }

    fn scan_identifier(&mut self) -> usize {
        while self.current_byte().is_some_and(is_identifier_continue) {
            self.position += 1;
        }
        self.position
    }

    fn skip_whitespace(&mut self) {
        while self
            .current_byte()
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            self.position += 1;
        }
    }

    fn current_byte(&self) -> Option<u8> {
        self.sql.as_bytes().get(self.position).copied()
    }

    fn syntax_error(&self, expected: &'static str) -> ParseError {
        self.syntax_error_at(expected, self.position, None)
    }

    fn syntax_error_at(
        &self,
        expected: &'static str,
        position: usize,
        known_end: Option<usize>,
    ) -> ParseError {
        let found = if position == self.sql.len() {
            None
        } else {
            let end = known_end.unwrap_or_else(|| {
                self.sql[position..]
                    .chars()
                    .next()
                    .map_or(position, |character| position + character.len_utf8())
            });
            Some(self.sql[position..end].to_owned())
        };
        ParseError::Syntax {
            position,
            expected,
            found,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NumberKind {
    Integer,
    Float,
}

fn classify_number(literal: &str) -> Option<NumberKind> {
    let bytes = literal.as_bytes();
    let mut position = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let integer_start = position;
    while bytes.get(position).is_some_and(u8::is_ascii_digit) {
        position += 1;
    }
    let mut digit_count = position - integer_start;
    let mut kind = NumberKind::Integer;

    if bytes.get(position) == Some(&b'.') {
        kind = NumberKind::Float;
        position += 1;
        let fraction_start = position;
        while bytes.get(position).is_some_and(u8::is_ascii_digit) {
            position += 1;
        }
        digit_count += position - fraction_start;
    }
    if digit_count == 0 {
        return None;
    }

    if matches!(bytes.get(position), Some(b'e' | b'E')) {
        kind = NumberKind::Float;
        position += 1;
        if matches!(bytes.get(position), Some(b'+' | b'-')) {
            position += 1;
        }
        let exponent_start = position;
        while bytes.get(position).is_some_and(u8::is_ascii_digit) {
            position += 1;
        }
        if position == exponent_start {
            return None;
        }
    }

    (position == bytes.len()).then_some(kind)
}

fn is_non_finite_name(literal: &str) -> bool {
    literal.eq_ignore_ascii_case("nan")
        || literal.eq_ignore_ascii_case("inf")
        || literal.eq_ignore_ascii_case("infinity")
}

const fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

const fn is_identifier_continue(byte: u8) -> bool {
    is_identifier_start(byte) || byte.is_ascii_digit()
}

const fn is_value_delimiter(byte: u8) -> bool {
    byte.is_ascii_whitespace() || matches!(byte, b',' | b')' | b';')
}
