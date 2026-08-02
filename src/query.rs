//! Parsing for the first executable SQL query shape.

use std::error::Error;
use std::fmt;

use crate::Value;
use crate::lexer::{Delimiter, LexError, LexerLimits, Literal, Operator, Token, TokenKind, lex};

/// The result of parsing a single scalar `SELECT` statement.
#[derive(Clone, Debug, PartialEq)]
pub struct ScalarSelect {
    column_name: String,
    value: Value,
}

impl ScalarSelect {
    /// Returns the output column name, either the alias or the literal spelling.
    #[must_use]
    pub fn column_name(&self) -> &str {
        &self.column_name
    }

    /// Returns the typed literal value.
    #[must_use]
    pub const fn value(&self) -> &Value {
        &self.value
    }

    /// Formats the value for text result formats.
    #[must_use]
    pub fn value_text(&self) -> String {
        match &self.value {
            Value::Int64(value) => value.to_string(),
            Value::Float64(value) => value.to_string(),
            Value::Bool(value) => value.to_string(),
            Value::String(value) => value.clone(),
        }
    }
}

/// An error returned while parsing the supported scalar `SELECT` shape.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScalarSelectError {
    /// Tokenization failed.
    Lex(LexError),
    /// The token sequence does not match the supported statement shape.
    Syntax {
        /// Zero-based byte position at which parsing stopped.
        position: usize,
        /// Description of the expected syntax.
        expected: &'static str,
    },
    /// More than one semicolon-delimited statement was supplied.
    MultipleStatements {
        /// Zero-based byte position at which the extra statement begins.
        position: usize,
    },
    /// An integer literal is outside the supported `Int64` range.
    InvalidInt64 {
        /// The rejected source spelling.
        literal: String,
        /// Zero-based byte position of the literal.
        position: usize,
    },
    /// A float literal cannot be represented by a finite `Float64`.
    InvalidFloat64 {
        /// The rejected source spelling.
        literal: String,
        /// Zero-based byte position of the literal.
        position: usize,
    },
    /// `NULL` is recognized lexically but is not yet executable.
    UnsupportedNull {
        /// Zero-based byte position of the literal.
        position: usize,
    },
}

impl fmt::Display for ScalarSelectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lex(error) => error.fmt(formatter),
            Self::Syntax { position, expected } => {
                write!(
                    formatter,
                    "SQL parse error at byte {position}: expected {expected}"
                )
            }
            Self::MultipleStatements { position } => write!(
                formatter,
                "SQL parse error at byte {position}: only one statement is allowed"
            ),
            Self::InvalidInt64 { literal, position } => write!(
                formatter,
                "SQL parse error at byte {position}: integer literal `{literal}` is outside the Int64 range"
            ),
            Self::InvalidFloat64 { literal, position } => write!(
                formatter,
                "SQL parse error at byte {position}: float literal `{literal}` is not a finite Float64"
            ),
            Self::UnsupportedNull { position } => write!(
                formatter,
                "SQL parse error at byte {position}: NULL literals are not supported"
            ),
        }
    }
}

impl Error for ScalarSelectError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Lex(error) => Some(error),
            _ => None,
        }
    }
}

impl From<LexError> for ScalarSelectError {
    fn from(error: LexError) -> Self {
        Self::Lex(error)
    }
}

/// Parses one `SELECT <literal> [AS identifier]` statement.
///
/// A single trailing semicolon is accepted. Keywords are ASCII
/// case-insensitive, and the lexer default limits bound the work performed.
pub fn parse_scalar_select(input: &str) -> Result<ScalarSelect, ScalarSelectError> {
    let tokens = lex(input, LexerLimits::default())?;
    let mut cursor = Cursor::new(input, &tokens);

    cursor.expect_keyword("SELECT", "SELECT")?;
    let expression_start = cursor.position();
    let sign = cursor.take_sign();
    let literal = cursor
        .next()
        .ok_or_else(|| cursor.syntax("a scalar literal"))?;
    let expression_end = literal.span.end;

    let value = parse_value(literal, sign)?;
    let mut column_name = input[expression_start..expression_end].to_owned();

    if cursor.peek_is_semicolon() {
        cursor.finish_semicolon()?;
    } else if !cursor.is_finished() {
        cursor.expect_keyword("AS", "AS or the end of the statement")?;
        column_name = cursor.take_alias()?;
        if cursor.peek_is_semicolon() {
            cursor.finish_semicolon()?;
        } else if !cursor.is_finished() {
            return Err(cursor.syntax("the end of the statement"));
        }
    }

    Ok(ScalarSelect { column_name, value })
}

fn parse_value(token: &Token, sign: Option<Operator>) -> Result<Value, ScalarSelectError> {
    match &token.kind {
        TokenKind::Literal(Literal::Number(number)) => parse_number(number, sign, token.span.start),
        TokenKind::Literal(Literal::String(value)) if sign.is_none() => {
            Ok(Value::String(value.clone()))
        }
        TokenKind::Literal(Literal::Boolean(value)) if sign.is_none() => Ok(Value::Bool(*value)),
        TokenKind::Literal(Literal::Null) if sign.is_none() => {
            Err(ScalarSelectError::UnsupportedNull {
                position: token.span.start,
            })
        }
        _ => Err(ScalarSelectError::Syntax {
            position: token.span.start,
            expected: "an Int64, Float64, Bool, or String literal",
        }),
    }
}

fn parse_number(
    number: &str,
    sign: Option<Operator>,
    position: usize,
) -> Result<Value, ScalarSelectError> {
    let sign = match sign {
        Some(Operator::Plus) => "+",
        Some(Operator::Minus) => "-",
        None => "",
        _ => unreachable!("only unary signs are passed to parse_number"),
    };
    let literal = format!("{sign}{number}");

    if number.contains(['.', 'e', 'E']) {
        let value = literal
            .parse::<f64>()
            .map_err(|_| ScalarSelectError::InvalidFloat64 {
                literal: literal.clone(),
                position,
            })?;
        if !value.is_finite() {
            return Err(ScalarSelectError::InvalidFloat64 { literal, position });
        }
        Ok(Value::Float64(value))
    } else {
        literal
            .parse::<i64>()
            .map(Value::Int64)
            .map_err(|_| ScalarSelectError::InvalidInt64 { literal, position })
    }
}

struct Cursor<'a> {
    input: &'a str,
    tokens: &'a [Token],
    index: usize,
}

impl<'a> Cursor<'a> {
    const fn new(input: &'a str, tokens: &'a [Token]) -> Self {
        Self {
            input,
            tokens,
            index: 0,
        }
    }

    fn next(&mut self) -> Option<&'a Token> {
        let token = self.tokens.get(self.index)?;
        self.index += 1;
        Some(token)
    }

    fn position(&self) -> usize {
        self.tokens
            .get(self.index)
            .map_or(self.input.len(), |token| token.span.start)
    }

    fn syntax(&self, expected: &'static str) -> ScalarSelectError {
        ScalarSelectError::Syntax {
            position: self.position(),
            expected,
        }
    }

    fn is_finished(&self) -> bool {
        self.index == self.tokens.len()
    }

    fn expect_keyword(
        &mut self,
        keyword: &str,
        expected: &'static str,
    ) -> Result<(), ScalarSelectError> {
        let token = self.next().ok_or_else(|| self.syntax(expected))?;
        match &token.kind {
            TokenKind::Identifier(identifier) if identifier.eq_ignore_ascii_case(keyword) => Ok(()),
            _ => Err(ScalarSelectError::Syntax {
                position: token.span.start,
                expected,
            }),
        }
    }

    fn take_sign(&mut self) -> Option<Operator> {
        let sign = match self.tokens.get(self.index).map(|token| &token.kind) {
            Some(TokenKind::Operator(operator @ (Operator::Plus | Operator::Minus))) => *operator,
            _ => return None,
        };
        self.index += 1;
        Some(sign)
    }

    fn take_alias(&mut self) -> Result<String, ScalarSelectError> {
        let token = self
            .next()
            .ok_or_else(|| self.syntax("an identifier after AS"))?;
        let alias = match &token.kind {
            TokenKind::Identifier(alias) | TokenKind::QuotedIdentifier(alias)
                if !alias.is_empty() =>
            {
                alias.clone()
            }
            _ => {
                return Err(ScalarSelectError::Syntax {
                    position: token.span.start,
                    expected: "an identifier after AS",
                });
            }
        };
        Ok(alias)
    }

    fn peek_is_semicolon(&self) -> bool {
        matches!(
            self.tokens.get(self.index).map(|token| &token.kind),
            Some(TokenKind::Delimiter(Delimiter::Semicolon))
        )
    }

    fn finish_semicolon(&mut self) -> Result<(), ScalarSelectError> {
        self.index += 1;
        if self.is_finished() {
            Ok(())
        } else {
            Err(ScalarSelectError::MultipleStatements {
                position: self.position(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_boundaries_and_a_quoted_alias() {
        let minimum = parse_scalar_select("SELECT -9223372036854775808 AS minimum;").unwrap();
        assert_eq!(minimum.value(), &Value::Int64(i64::MIN));
        assert_eq!(minimum.column_name(), "minimum");

        let float = parse_scalar_select("select +.5e2 as \"Daily Total\"").unwrap();
        assert_eq!(float.value(), &Value::Float64(50.0));
        assert_eq!(float.column_name(), "Daily Total");
    }

    #[test]
    fn uses_the_literal_spelling_without_an_alias() {
        let query = parse_scalar_select("SELECT 'customer''s note'").unwrap();

        assert_eq!(query.column_name(), "'customer''s note'");
        assert_eq!(query.value(), &Value::String("customer's note".into()));
    }

    #[test]
    fn rejects_out_of_range_and_trailing_syntax() {
        assert!(matches!(
            parse_scalar_select("SELECT 9223372036854775808"),
            Err(ScalarSelectError::InvalidInt64 { .. })
        ));
        assert!(matches!(
            parse_scalar_select("SELECT 1e999"),
            Err(ScalarSelectError::InvalidFloat64 { .. })
        ));
        assert!(matches!(
            parse_scalar_select("SELECT 1; SELECT 2"),
            Err(ScalarSelectError::MultipleStatements { .. })
        ));
        assert!(parse_scalar_select("SELECT 1 + 2").is_err());
        assert!(parse_scalar_select("SELECT NULL").is_err());
    }
}
