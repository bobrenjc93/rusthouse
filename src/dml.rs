//! Parsing and execution for `INSERT INTO ... VALUES` statements.

use std::error::Error;
use std::fmt;

use crate::catalog::{Catalog, TableNotFoundError};
use crate::lexer::{Delimiter, LexError, LexerLimits, Literal, Operator, Token, TokenKind, lex};
use crate::storage::{BatchInsertError, Value};

/// A parsed `INSERT INTO ... VALUES` statement containing one or more rows.
#[derive(Clone, Debug, PartialEq)]
pub struct InsertValuesStatement {
    table_name: String,
    rows: Vec<Vec<Value>>,
}

impl InsertValuesStatement {
    /// Returns the target table name exactly as parsed.
    #[must_use]
    pub fn table_name(&self) -> &str {
        &self.table_name
    }

    /// Returns the typed values in the first row.
    ///
    /// Every statement contains at least one row. Use [`Self::rows`] to inspect
    /// every row in a multi-row statement.
    #[must_use]
    pub fn values(&self) -> &[Value] {
        &self.rows[0]
    }

    /// Returns all typed rows in statement order.
    #[must_use]
    pub fn rows(&self) -> &[Vec<Value>] {
        &self.rows
    }

    fn into_parts(self) -> (String, Vec<Vec<Value>>) {
        (self.table_name, self.rows)
    }
}

/// An error returned while parsing or executing `INSERT VALUES`.
#[derive(Clone, Debug, PartialEq)]
pub enum InsertValuesError {
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
        /// Zero-based byte position of the numeric token.
        position: usize,
    },
    /// A float literal cannot be represented by a finite `Float64`.
    InvalidFloat64 {
        /// The rejected source spelling.
        literal: String,
        /// Zero-based byte position of the numeric token.
        position: usize,
    },
    /// `NULL` is recognized lexically but is not supported by storage.
    UnsupportedNull {
        /// Zero-based byte position of the literal.
        position: usize,
    },
    /// The target table is not present in the catalog.
    TableNotFound(TableNotFoundError),
    /// A typed row does not match the target table schema.
    Insert(BatchInsertError),
}

impl fmt::Display for InsertValuesError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lex(error) => error.fmt(formatter),
            Self::Syntax { position, expected } => write!(
                formatter,
                "SQL parse error at byte {position}: expected {expected}"
            ),
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
            Self::TableNotFound(error) => error.fmt(formatter),
            Self::Insert(error) => write!(formatter, "batch insertion failed: {error}"),
        }
    }
}

impl Error for InsertValuesError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Lex(error) => Some(error),
            Self::TableNotFound(error) => Some(error),
            Self::Insert(error) => Some(error),
            _ => None,
        }
    }
}

impl From<LexError> for InsertValuesError {
    fn from(error: LexError) -> Self {
        Self::Lex(error)
    }
}

impl From<TableNotFoundError> for InsertValuesError {
    fn from(error: TableNotFoundError) -> Self {
        Self::TableNotFound(error)
    }
}

impl From<BatchInsertError> for InsertValuesError {
    fn from(error: BatchInsertError) -> Self {
        Self::Insert(error)
    }
}

/// Parses exactly one `INSERT INTO name VALUES (literal [, literal ...])
/// [, (literal [, literal ...]) ...]` statement.
///
/// The statement keywords are ASCII case-insensitive. A single trailing
/// semicolon is optional, and the lexer default limits bound the work
/// performed. Numeric signs are accepted as part of numeric literals.
pub fn parse_insert_values(input: &str) -> Result<InsertValuesStatement, InsertValuesError> {
    let tokens = lex(input, LexerLimits::default())?;
    let mut cursor = Cursor::new(input, &tokens);

    cursor.expect_keyword("INSERT", "INSERT")?;
    cursor.expect_keyword("INTO", "INTO")?;
    let table_name = cursor.take_identifier("a table name")?;
    cursor.expect_keyword("VALUES", "VALUES")?;
    let mut rows = Vec::new();
    loop {
        rows.push(cursor.take_row()?);
        if cursor.take_delimiter(Delimiter::Comma) {
            continue;
        }
        break;
    }
    cursor.finish()?;

    Ok(InsertValuesStatement { table_name, rows })
}

/// Parses and inserts one or more rows into an existing catalog table.
///
/// Parsing completes before catalog lookup. The table's atomic batch insertion
/// validates every row's width, exact logical types, and finite floats before
/// any physical column is changed.
pub fn execute_insert_values(catalog: &mut Catalog, input: &str) -> Result<(), InsertValuesError> {
    let statement = parse_insert_values(input)?;
    let (table_name, rows) = statement.into_parts();
    catalog.table_mut(&table_name)?.insert_batch(rows)?;
    Ok(())
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

    fn position(&self) -> usize {
        self.tokens
            .get(self.index)
            .map_or(self.input.len(), |token| token.span.start)
    }

    fn syntax(&self, expected: &'static str) -> InsertValuesError {
        InsertValuesError::Syntax {
            position: self.position(),
            expected,
        }
    }

    fn next(&mut self) -> Option<&'a Token> {
        let token = self.tokens.get(self.index)?;
        self.index += 1;
        Some(token)
    }

    fn expect_keyword(
        &mut self,
        keyword: &str,
        expected: &'static str,
    ) -> Result<(), InsertValuesError> {
        let token = self.next().ok_or_else(|| self.syntax(expected))?;
        match &token.kind {
            TokenKind::Identifier(identifier) if identifier.eq_ignore_ascii_case(keyword) => Ok(()),
            _ => Err(InsertValuesError::Syntax {
                position: token.span.start,
                expected,
            }),
        }
    }

    fn take_identifier(&mut self, expected: &'static str) -> Result<String, InsertValuesError> {
        let token = self.next().ok_or_else(|| self.syntax(expected))?;
        match &token.kind {
            TokenKind::Identifier(identifier) | TokenKind::QuotedIdentifier(identifier)
                if !identifier.is_empty() =>
            {
                Ok(identifier.clone())
            }
            _ => Err(InsertValuesError::Syntax {
                position: token.span.start,
                expected,
            }),
        }
    }

    fn take_value(&mut self) -> Result<Value, InsertValuesError> {
        let sign = self.take_sign();
        let token = self
            .next()
            .ok_or_else(|| self.syntax("an Int64, Float64, Bool, or String literal"))?;
        parse_value(token, sign)
    }

    fn take_row(&mut self) -> Result<Vec<Value>, InsertValuesError> {
        self.expect_delimiter(Delimiter::LeftParenthesis, "`(`")?;
        let mut values = Vec::new();
        loop {
            values.push(self.take_value()?);
            if self.take_delimiter(Delimiter::Comma) {
                continue;
            }
            self.expect_delimiter(Delimiter::RightParenthesis, "`,` or `)`")?;
            return Ok(values);
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

    fn expect_delimiter(
        &mut self,
        delimiter: Delimiter,
        expected: &'static str,
    ) -> Result<(), InsertValuesError> {
        let token = self.next().ok_or_else(|| self.syntax(expected))?;
        if token.kind == TokenKind::Delimiter(delimiter) {
            Ok(())
        } else {
            Err(InsertValuesError::Syntax {
                position: token.span.start,
                expected,
            })
        }
    }

    fn take_delimiter(&mut self, delimiter: Delimiter) -> bool {
        if self
            .tokens
            .get(self.index)
            .is_some_and(|token| token.kind == TokenKind::Delimiter(delimiter))
        {
            self.index += 1;
            true
        } else {
            false
        }
    }

    fn finish(&mut self) -> Result<(), InsertValuesError> {
        if self.take_delimiter(Delimiter::Semicolon) {
            if self.index != self.tokens.len() {
                return Err(InsertValuesError::MultipleStatements {
                    position: self.position(),
                });
            }
        } else if self.index != self.tokens.len() {
            return Err(self.syntax("`;` or the end of the statement"));
        }
        Ok(())
    }
}

fn parse_value(token: &Token, sign: Option<Operator>) -> Result<Value, InsertValuesError> {
    match &token.kind {
        TokenKind::Literal(Literal::Number(number)) => parse_number(number, sign, token.span.start),
        TokenKind::Literal(Literal::String(value)) if sign.is_none() => {
            Ok(Value::String(value.clone()))
        }
        TokenKind::Literal(Literal::Boolean(value)) if sign.is_none() => Ok(Value::Bool(*value)),
        TokenKind::Literal(Literal::Null) if sign.is_none() => {
            Err(InsertValuesError::UnsupportedNull {
                position: token.span.start,
            })
        }
        _ => Err(InsertValuesError::Syntax {
            position: token.span.start,
            expected: "an Int64, Float64, Bool, or String literal",
        }),
    }
}

fn parse_number(
    number: &str,
    sign: Option<Operator>,
    position: usize,
) -> Result<Value, InsertValuesError> {
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
            .map_err(|_| InsertValuesError::InvalidFloat64 {
                literal: literal.clone(),
                position,
            })?;
        if !value.is_finite() {
            return Err(InsertValuesError::InvalidFloat64 { literal, position });
        }
        Ok(Value::Float64(value))
    } else {
        literal
            .parse::<i64>()
            .map(Value::Int64)
            .map_err(|_| InsertValuesError::InvalidInt64 { literal, position })
    }
}
