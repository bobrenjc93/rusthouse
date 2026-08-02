//! Parsing and execution for the supported `SELECT` statement shapes.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;

use crate::catalog::{Catalog, TableNotFoundError};
use crate::lexer::{Delimiter, LexError, LexerLimits, Literal, Operator, Token, TokenKind, lex};
use crate::storage::{Column, ColumnSchema, Table, Value};

/// Maximum estimated heap allocation for one materialized table projection.
pub const MAX_TABLE_SELECT_RESULT_BYTES: usize = 64 * 1024 * 1024;

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

/// The materialized result of a table query.
///
/// Projection headers retain each catalog column's logical type and rows
/// follow insertion order. Aggregate results describe their own typed schema.
#[derive(Clone, Debug, PartialEq)]
pub struct TableSelectResult {
    headers: Vec<ColumnSchema>,
    rows: Vec<Vec<Value>>,
}

impl TableSelectResult {
    /// Returns the projected names and logical types in query order.
    #[must_use]
    pub fn headers(&self) -> &[ColumnSchema] {
        &self.headers
    }

    /// Returns the materialized rows in table insertion order.
    #[must_use]
    pub fn rows(&self) -> &[Vec<Value>] {
        &self.rows
    }

    /// Consumes the result and returns its materialized rows.
    #[must_use]
    pub fn into_rows(self) -> Vec<Vec<Value>> {
        self.rows
    }
}

/// A column requested by a projection does not exist in its source table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ColumnNotFoundError {
    /// The exact table name resolved by the query.
    pub table_name: String,
    /// The exact requested column name.
    pub column_name: String,
}

impl fmt::Display for ColumnNotFoundError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "column `{}` was not found in table `{}`",
            self.column_name, self.table_name
        )
    }
}

impl Error for ColumnNotFoundError {}

/// An error returned while parsing or executing a table projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TableSelectError {
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
    /// The source table is not present in the catalog.
    TableNotFound(TableNotFoundError),
    /// A projected column is not present in the source table.
    ColumnNotFound(ColumnNotFoundError),
    /// The table row count cannot be represented by the aggregate's `Int64` type.
    CountOutOfRange {
        /// The metadata row count that could not be converted.
        count: usize,
    },
    /// Materializing the complete result would exceed the memory budget.
    ResultSizeLimitExceeded {
        /// Estimated bytes required by the materialized headers and rows.
        estimated_bytes: usize,
        /// Maximum estimated bytes allowed for one result.
        limit: usize,
    },
}

impl fmt::Display for TableSelectError {
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
            Self::TableNotFound(error) => error.fmt(formatter),
            Self::ColumnNotFound(error) => error.fmt(formatter),
            Self::CountOutOfRange { count } => {
                write!(
                    formatter,
                    "table row count {count} is outside the Int64 range"
                )
            }
            Self::ResultSizeLimitExceeded {
                estimated_bytes,
                limit,
            } => write!(
                formatter,
                "query result requires an estimated {estimated_bytes} bytes, limit is {limit}"
            ),
        }
    }
}

impl Error for TableSelectError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Lex(error) => Some(error),
            Self::TableNotFound(error) => Some(error),
            Self::ColumnNotFound(error) => Some(error),
            _ => None,
        }
    }
}

impl From<LexError> for TableSelectError {
    fn from(error: LexError) -> Self {
        Self::Lex(error)
    }
}

impl From<TableNotFoundError> for TableSelectError {
    fn from(error: TableNotFoundError) -> Self {
        Self::TableNotFound(error)
    }
}

/// Executes a supported table `SELECT` against a catalog.
///
/// Supported shapes are `SELECT column [, column ...] FROM table` and exactly
/// `SELECT COUNT(*) [AS identifier] FROM table`. One optional trailing
/// semicolon is accepted. Names are resolved exactly and case-sensitively,
/// matching the catalog and schema storage boundaries. A count reads only the
/// table's row-count metadata and validates that it fits its `Int64` result.
pub fn execute_table_select(
    catalog: &Catalog,
    input: &str,
) -> Result<TableSelectResult, TableSelectError> {
    let projection = parse_table_select(input)?;
    let table = catalog.table(&projection.table_name)?;
    let column_names = match projection.selection {
        TableSelection::Columns(column_names) => column_names,
        TableSelection::Count { column_name } => return count_result(table, column_name),
    };
    let column_indices: HashMap<&str, usize> = table
        .schema()
        .columns()
        .iter()
        .enumerate()
        .map(|(index, column)| (column.name(), index))
        .collect();

    let mut columns = Vec::with_capacity(column_names.len());
    for column_name in &column_names {
        let Some(column_index) = column_indices.get(column_name.as_str()).copied() else {
            return Err(TableSelectError::ColumnNotFound(ColumnNotFoundError {
                table_name: projection.table_name,
                column_name: column_name.clone(),
            }));
        };

        columns.push(ResolvedColumn {
            index: column_index,
            schema: table
                .schema()
                .column(column_index)
                .expect("resolved schema column index remains valid"),
            values: table
                .column(column_index)
                .expect("table columns correspond to schema columns"),
        });
    }

    enforce_result_size_limit(table, &columns, MAX_TABLE_SELECT_RESULT_BYTES)?;

    let headers = columns.iter().map(|column| column.schema.clone()).collect();
    let mut rows = Vec::with_capacity(table.row_count());
    for row_index in 0..table.row_count() {
        rows.push(
            columns
                .iter()
                .map(|column| value_at(column.values, row_index))
                .collect(),
        );
    }

    Ok(TableSelectResult { headers, rows })
}

fn count_result(table: &Table, column_name: String) -> Result<TableSelectResult, TableSelectError> {
    let count = int64_count(table.row_count())?;
    Ok(TableSelectResult {
        headers: vec![ColumnSchema::new(
            column_name,
            crate::storage::DataType::Int64,
        )],
        rows: vec![vec![Value::Int64(count)]],
    })
}

fn int64_count(count: usize) -> Result<i64, TableSelectError> {
    i64::try_from(count).map_err(|_| TableSelectError::CountOutOfRange { count })
}

struct ResolvedColumn<'a> {
    index: usize,
    schema: &'a ColumnSchema,
    values: &'a Column,
}

fn enforce_result_size_limit(
    table: &Table,
    columns: &[ResolvedColumn<'_>],
    limit: usize,
) -> Result<(), TableSelectError> {
    let estimated_bytes = estimate_result_bytes(table, columns);
    if estimated_bytes > limit {
        Err(TableSelectError::ResultSizeLimitExceeded {
            estimated_bytes,
            limit,
        })
    } else {
        Ok(())
    }
}

fn estimate_result_bytes(table: &Table, columns: &[ResolvedColumn<'_>]) -> usize {
    let header_bytes = columns
        .len()
        .saturating_mul(std::mem::size_of::<ColumnSchema>())
        .saturating_add(
            columns
                .iter()
                .map(|column| column.schema.name().len())
                .fold(0usize, usize::saturating_add),
        );
    let row_vector_bytes = table
        .row_count()
        .saturating_mul(std::mem::size_of::<Vec<Value>>());
    let row_value_bytes = table
        .row_count()
        .saturating_mul(columns.len())
        .saturating_mul(std::mem::size_of::<Value>());

    let mut string_bytes_by_column = HashMap::new();
    let string_payload_bytes = columns.iter().fold(0usize, |total, column| {
        let bytes = *string_bytes_by_column
            .entry(column.index)
            .or_insert_with(|| {
                let bytes = match column.values {
                    Column::String(values) => values
                        .iter()
                        .map(String::len)
                        .fold(0usize, usize::saturating_add),
                    _ => 0,
                };
                bytes
            });
        total.saturating_add(bytes)
    });

    header_bytes
        .saturating_add(row_vector_bytes)
        .saturating_add(row_value_bytes)
        .saturating_add(string_payload_bytes)
}

struct TableProjection {
    selection: TableSelection,
    table_name: String,
}

enum TableSelection {
    Columns(Vec<String>),
    Count { column_name: String },
}

fn parse_table_select(input: &str) -> Result<TableProjection, TableSelectError> {
    let tokens = lex(input, LexerLimits::default())?;
    let mut cursor = TableSelectCursor::new(input, &tokens);

    cursor.expect_keyword("SELECT", "SELECT")?;
    let selection = if cursor.peek_is_count_call() {
        let expression_start = cursor.position();
        cursor.expect_keyword("COUNT", "COUNT")?;
        cursor.expect_delimiter(Delimiter::LeftParenthesis, "`(` after COUNT")?;
        cursor.expect_operator(Operator::Multiply, "`*` in COUNT(*)")?;
        let expression_end =
            cursor.expect_delimiter(Delimiter::RightParenthesis, "`)` after COUNT(*)")?;
        let mut column_name = input[expression_start..expression_end].to_owned();
        if cursor.take_keyword("AS") {
            column_name = cursor.take_identifier("an identifier after AS")?;
        }
        TableSelection::Count { column_name }
    } else {
        let mut column_names = vec![cursor.take_identifier("a column name")?];
        while cursor.take_comma() {
            column_names.push(cursor.take_identifier("a column name after `,`")?);
        }
        TableSelection::Columns(column_names)
    };
    cursor.expect_keyword("FROM", "`,` or FROM")?;
    let table_name = cursor.take_identifier("a table name after FROM")?;
    cursor.finish()?;

    Ok(TableProjection {
        selection,
        table_name,
    })
}

fn value_at(column: &Column, row_index: usize) -> Value {
    match column {
        Column::Int64(values) => Value::Int64(values[row_index]),
        Column::Float64(values) => Value::Float64(values[row_index]),
        Column::Bool(values) => Value::Bool(values[row_index]),
        Column::String(values) => Value::String(values[row_index].clone()),
    }
}

struct TableSelectCursor<'a> {
    input: &'a str,
    tokens: &'a [Token],
    index: usize,
}

impl<'a> TableSelectCursor<'a> {
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

    fn syntax(&self, expected: &'static str) -> TableSelectError {
        TableSelectError::Syntax {
            position: self.position(),
            expected,
        }
    }

    fn peek_is_count_call(&self) -> bool {
        matches!(
            (
                self.tokens.get(self.index).map(|token| &token.kind),
                self.tokens.get(self.index + 1).map(|token| &token.kind),
            ),
            (
                Some(TokenKind::Identifier(identifier)),
                Some(TokenKind::Delimiter(Delimiter::LeftParenthesis)),
            ) if identifier.eq_ignore_ascii_case("COUNT")
        )
    }

    fn take_keyword(&mut self, keyword: &str) -> bool {
        if matches!(
            self.tokens.get(self.index).map(|token| &token.kind),
            Some(TokenKind::Identifier(identifier)) if identifier.eq_ignore_ascii_case(keyword)
        ) {
            self.index += 1;
            true
        } else {
            false
        }
    }

    fn expect_keyword(
        &mut self,
        keyword: &str,
        expected: &'static str,
    ) -> Result<(), TableSelectError> {
        let token = self
            .tokens
            .get(self.index)
            .ok_or_else(|| self.syntax(expected))?;
        match &token.kind {
            TokenKind::Identifier(identifier) if identifier.eq_ignore_ascii_case(keyword) => {
                self.index += 1;
                Ok(())
            }
            _ => Err(TableSelectError::Syntax {
                position: token.span.start,
                expected,
            }),
        }
    }

    fn take_identifier(&mut self, expected: &'static str) -> Result<String, TableSelectError> {
        let token = self
            .tokens
            .get(self.index)
            .ok_or_else(|| self.syntax(expected))?;
        let identifier = match &token.kind {
            TokenKind::Identifier(identifier) | TokenKind::QuotedIdentifier(identifier)
                if !identifier.is_empty() =>
            {
                identifier.clone()
            }
            _ => {
                return Err(TableSelectError::Syntax {
                    position: token.span.start,
                    expected,
                });
            }
        };
        self.index += 1;
        Ok(identifier)
    }

    fn expect_delimiter(
        &mut self,
        delimiter: Delimiter,
        expected: &'static str,
    ) -> Result<usize, TableSelectError> {
        let token = self
            .tokens
            .get(self.index)
            .ok_or_else(|| self.syntax(expected))?;
        if token.kind != TokenKind::Delimiter(delimiter) {
            return Err(TableSelectError::Syntax {
                position: token.span.start,
                expected,
            });
        }
        self.index += 1;
        Ok(token.span.end)
    }

    fn expect_operator(
        &mut self,
        operator: Operator,
        expected: &'static str,
    ) -> Result<(), TableSelectError> {
        let token = self
            .tokens
            .get(self.index)
            .ok_or_else(|| self.syntax(expected))?;
        if token.kind != TokenKind::Operator(operator) {
            return Err(TableSelectError::Syntax {
                position: token.span.start,
                expected,
            });
        }
        self.index += 1;
        Ok(())
    }

    fn take_comma(&mut self) -> bool {
        if matches!(
            self.tokens.get(self.index).map(|token| &token.kind),
            Some(TokenKind::Delimiter(Delimiter::Comma))
        ) {
            self.index += 1;
            true
        } else {
            false
        }
    }

    fn finish(&mut self) -> Result<(), TableSelectError> {
        if matches!(
            self.tokens.get(self.index).map(|token| &token.kind),
            Some(TokenKind::Delimiter(Delimiter::Semicolon))
        ) {
            self.index += 1;
            if self.index != self.tokens.len() {
                return Err(TableSelectError::MultipleStatements {
                    position: self.position(),
                });
            }
        } else if self.index != self.tokens.len() {
            return Err(self.syntax("`;` or the end of the statement"));
        }
        Ok(())
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

    #[test]
    fn result_size_accounting_enforces_the_exact_boundary() {
        let schema =
            crate::Schema::new(vec![ColumnSchema::new("payload", crate::DataType::String)])
                .unwrap();
        let mut table = Table::new(schema);
        table
            .insert_row(vec![Value::String("boundary".to_owned())])
            .unwrap();
        let columns = [ResolvedColumn {
            index: 0,
            schema: table.schema().column(0).unwrap(),
            values: table.column(0).unwrap(),
        }];
        let estimated_bytes = estimate_result_bytes(&table, &columns);

        assert_eq!(
            enforce_result_size_limit(&table, &columns, estimated_bytes),
            Ok(())
        );
        assert_eq!(
            enforce_result_size_limit(&table, &columns, estimated_bytes - 1),
            Err(TableSelectError::ResultSizeLimitExceeded {
                estimated_bytes,
                limit: estimated_bytes - 1,
            })
        );
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn count_conversion_rejects_values_outside_int64() {
        let count = usize::try_from(i64::MAX).unwrap() + 1;

        assert_eq!(
            int64_count(count),
            Err(TableSelectError::CountOutOfRange { count })
        );
    }
}
