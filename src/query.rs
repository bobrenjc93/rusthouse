//! Parsing and execution for the supported `SELECT` statement shapes.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;

use crate::catalog::{Catalog, TableNotFoundError};
use crate::lexer::{Delimiter, LexError, LexerLimits, Literal, Operator, Token, TokenKind, lex};
use crate::storage::{Column, ColumnSchema, DataType, Table, Value};

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

/// The materialized result of a table projection.
///
/// Headers follow projection order and retain each catalog column's logical
/// type. Rows follow insertion order and contain values in header order.
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

/// A column requested by a projection or predicate does not exist.
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
    /// An integer predicate literal is outside the supported `Int64` range.
    InvalidInt64 {
        /// The rejected source spelling.
        literal: String,
        /// Zero-based byte position of the literal.
        position: usize,
    },
    /// A float predicate literal cannot be represented by a finite `Float64`.
    InvalidFloat64 {
        /// The rejected source spelling.
        literal: String,
        /// Zero-based byte position of the literal.
        position: usize,
    },
    /// `NULL` is recognized lexically but is not supported by storage.
    UnsupportedNull {
        /// Zero-based byte position of the literal.
        position: usize,
    },
    /// The source table is not present in the catalog.
    TableNotFound(TableNotFoundError),
    /// A projected or predicate column is not present in the source table.
    ColumnNotFound(ColumnNotFoundError),
    /// A predicate literal does not exactly match its column's logical type.
    PredicateTypeMismatch {
        /// The predicate column name exactly as parsed.
        column_name: String,
        /// The logical type required by the predicate column.
        expected: DataType,
        /// The logical type inferred from the predicate literal.
        actual: DataType,
    },
    /// Materializing the complete result would exceed the memory budget.
    ResultSizeLimitExceeded {
        /// Estimated bytes known to be required when the limit was exceeded.
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
            Self::ColumnNotFound(error) => error.fmt(formatter),
            Self::PredicateTypeMismatch {
                column_name,
                expected,
                actual,
            } => write!(
                formatter,
                "predicate column `{column_name}` has type {expected}, but the literal has type {actual}"
            ),
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

/// Executes `SELECT column [, column ...] FROM table [WHERE column = literal]`.
///
/// One optional trailing semicolon is accepted. Names are resolved exactly
/// and case-sensitively, matching the catalog and schema storage boundaries.
/// Projection and predicate columns are resolved independently before any
/// result rows are materialized. Predicate literals require an exact logical
/// type match; numeric coercions are not performed.
pub fn execute_table_select(
    catalog: &Catalog,
    input: &str,
) -> Result<TableSelectResult, TableSelectError> {
    let projection = parse_table_select(input)?;
    let table = catalog.table(&projection.table_name)?;
    let column_indices: HashMap<&str, usize> = table
        .schema()
        .columns()
        .iter()
        .enumerate()
        .map(|(index, column)| (column.name(), index))
        .collect();

    let mut columns = Vec::with_capacity(projection.column_names.len());
    for column_name in &projection.column_names {
        let Some(column_index) = column_indices.get(column_name.as_str()).copied() else {
            return Err(TableSelectError::ColumnNotFound(ColumnNotFoundError {
                table_name: projection.table_name.clone(),
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

    let predicate = if let Some(predicate) = projection.predicate {
        let Some(column_index) = column_indices.get(predicate.column_name.as_str()).copied() else {
            return Err(TableSelectError::ColumnNotFound(ColumnNotFoundError {
                table_name: projection.table_name.clone(),
                column_name: predicate.column_name,
            }));
        };
        let column_type = table
            .schema()
            .column(column_index)
            .expect("resolved predicate column index remains valid")
            .data_type();
        let literal_type = predicate.value.data_type();
        if column_type != literal_type {
            return Err(TableSelectError::PredicateTypeMismatch {
                column_name: predicate.column_name,
                expected: column_type,
                actual: literal_type,
            });
        }
        Some(ResolvedPredicate {
            values: table
                .column(column_index)
                .expect("table columns correspond to schema columns"),
            value: predicate.value,
        })
    } else {
        None
    };

    let matching_row_count = enforce_result_size_limit(
        table,
        &columns,
        predicate.as_ref(),
        MAX_TABLE_SELECT_RESULT_BYTES,
    )?;

    let headers = columns.iter().map(|column| column.schema.clone()).collect();
    let mut rows = Vec::with_capacity(matching_row_count);
    for row_index in 0..table.row_count() {
        if !matches_predicate(predicate.as_ref(), row_index) {
            continue;
        }
        rows.push(
            columns
                .iter()
                .map(|column| value_at(column.values, row_index))
                .collect(),
        );
    }

    Ok(TableSelectResult { headers, rows })
}

struct ResolvedColumn<'a> {
    index: usize,
    schema: &'a ColumnSchema,
    values: &'a Column,
}

struct ResolvedPredicate<'a> {
    values: &'a Column,
    value: Value,
}

fn enforce_result_size_limit(
    table: &Table,
    columns: &[ResolvedColumn<'_>],
    predicate: Option<&ResolvedPredicate<'_>>,
    limit: usize,
) -> Result<usize, TableSelectError> {
    account_result_bytes(table, columns, predicate, limit)
        .map(|(_, matching_row_count)| matching_row_count)
}

fn account_result_bytes(
    table: &Table,
    columns: &[ResolvedColumn<'_>],
    predicate: Option<&ResolvedPredicate<'_>>,
    limit: usize,
) -> Result<(usize, usize), TableSelectError> {
    let header_bytes = columns
        .len()
        .saturating_mul(std::mem::size_of::<ColumnSchema>())
        .saturating_add(
            columns
                .iter()
                .map(|column| column.schema.name().len())
                .fold(0usize, usize::saturating_add),
        );
    let mut estimated_bytes = add_result_bytes(0, header_bytes, limit)?;
    let fixed_bytes_per_row = std::mem::size_of::<Vec<Value>>()
        .saturating_add(columns.len().saturating_mul(std::mem::size_of::<Value>()));

    if predicate.is_none() {
        estimated_bytes = add_result_bytes(
            estimated_bytes,
            table.row_count().saturating_mul(fixed_bytes_per_row),
            limit,
        )?;

        let projected_string_columns = projected_string_columns(columns);
        for (values, occurrences) in projected_string_columns.values() {
            for value in *values {
                estimated_bytes = add_result_bytes(
                    estimated_bytes,
                    value.len().saturating_mul(*occurrences),
                    limit,
                )?;
            }
        }
        return Ok((estimated_bytes, table.row_count()));
    }

    let projected_string_columns = projected_string_columns(columns);
    let mut matching_row_count = 0usize;
    for row_index in 0..table.row_count() {
        if !matches_predicate(predicate, row_index) {
            continue;
        }
        matching_row_count = matching_row_count.saturating_add(1);
        estimated_bytes = add_result_bytes(estimated_bytes, fixed_bytes_per_row, limit)?;
        for (values, occurrences) in projected_string_columns.values() {
            estimated_bytes = add_result_bytes(
                estimated_bytes,
                values[row_index].len().saturating_mul(*occurrences),
                limit,
            )?;
        }
    }

    Ok((estimated_bytes, matching_row_count))
}

fn projected_string_columns<'a>(
    columns: &[ResolvedColumn<'a>],
) -> HashMap<usize, (&'a [String], usize)> {
    let mut projected_string_columns = HashMap::new();
    for column in columns {
        if let Column::String(values) = column.values {
            let (_, occurrences) = projected_string_columns
                .entry(column.index)
                .or_insert((values.as_slice(), 0usize));
            *occurrences = occurrences.saturating_add(1);
        }
    }
    projected_string_columns
}

fn add_result_bytes(
    estimated_bytes: usize,
    additional_bytes: usize,
    limit: usize,
) -> Result<usize, TableSelectError> {
    let estimated_bytes = estimated_bytes.saturating_add(additional_bytes);
    if estimated_bytes > limit {
        Err(TableSelectError::ResultSizeLimitExceeded {
            estimated_bytes,
            limit,
        })
    } else {
        Ok(estimated_bytes)
    }
}

struct TableProjection {
    column_names: Vec<String>,
    table_name: String,
    predicate: Option<EqualityPredicate>,
}

struct EqualityPredicate {
    column_name: String,
    value: Value,
}

fn parse_table_select(input: &str) -> Result<TableProjection, TableSelectError> {
    let tokens = lex(input, LexerLimits::default())?;
    let mut cursor = TableSelectCursor::new(input, &tokens);

    cursor.expect_keyword("SELECT", "SELECT")?;
    let mut column_names = vec![cursor.take_identifier("a column name")?];
    while cursor.take_comma() {
        column_names.push(cursor.take_identifier("a column name after `,`")?);
    }
    cursor.expect_keyword("FROM", "`,` or FROM")?;
    let table_name = cursor.take_identifier("a table name after FROM")?;
    let predicate = if cursor.take_keyword("WHERE") {
        let column_name = cursor.take_identifier("a column name after WHERE")?;
        cursor.expect_operator(Operator::Equal, "`=` after the predicate column")?;
        let value = cursor.take_predicate_value()?;
        Some(EqualityPredicate { column_name, value })
    } else {
        None
    };
    cursor.finish()?;

    Ok(TableProjection {
        column_names,
        table_name,
        predicate,
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

fn matches_predicate(predicate: Option<&ResolvedPredicate<'_>>, row_index: usize) -> bool {
    let Some(predicate) = predicate else {
        return true;
    };

    match (predicate.values, &predicate.value) {
        (Column::Int64(values), Value::Int64(value)) => values[row_index] == *value,
        (Column::Float64(values), Value::Float64(value)) => values[row_index] == *value,
        (Column::Bool(values), Value::Bool(value)) => values[row_index] == *value,
        (Column::String(values), Value::String(value)) => values[row_index] == *value,
        _ => unreachable!("predicate type is validated before scanning rows"),
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

    fn expect_operator(
        &mut self,
        operator: Operator,
        expected: &'static str,
    ) -> Result<(), TableSelectError> {
        let token = self
            .tokens
            .get(self.index)
            .ok_or_else(|| self.syntax(expected))?;
        if token.kind == TokenKind::Operator(operator) {
            self.index += 1;
            Ok(())
        } else {
            Err(TableSelectError::Syntax {
                position: token.span.start,
                expected,
            })
        }
    }

    fn take_predicate_value(&mut self) -> Result<Value, TableSelectError> {
        let sign = match self.tokens.get(self.index).map(|token| &token.kind) {
            Some(TokenKind::Operator(operator @ (Operator::Plus | Operator::Minus))) => {
                self.index += 1;
                Some(*operator)
            }
            _ => None,
        };
        let token = self
            .tokens
            .get(self.index)
            .ok_or_else(|| self.syntax("an Int64, Float64, Bool, or String literal"))?;
        self.index += 1;
        parse_predicate_value(token, sign)
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

fn parse_predicate_value(token: &Token, sign: Option<Operator>) -> Result<Value, TableSelectError> {
    parse_value(token, sign).map_err(|error| match error {
        ScalarSelectError::Lex(error) => TableSelectError::Lex(error),
        ScalarSelectError::Syntax { position, expected } => {
            TableSelectError::Syntax { position, expected }
        }
        ScalarSelectError::MultipleStatements { position } => {
            TableSelectError::MultipleStatements { position }
        }
        ScalarSelectError::InvalidInt64 { literal, position } => {
            TableSelectError::InvalidInt64 { literal, position }
        }
        ScalarSelectError::InvalidFloat64 { literal, position } => {
            TableSelectError::InvalidFloat64 { literal, position }
        }
        ScalarSelectError::UnsupportedNull { position } => {
            TableSelectError::UnsupportedNull { position }
        }
    })
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
        let estimated_bytes = account_result_bytes(&table, &columns, None, usize::MAX)
            .unwrap()
            .0;

        assert_eq!(
            enforce_result_size_limit(&table, &columns, None, estimated_bytes),
            Ok(1)
        );
        assert_eq!(
            enforce_result_size_limit(&table, &columns, None, estimated_bytes - 1),
            Err(TableSelectError::ResultSizeLimitExceeded {
                estimated_bytes,
                limit: estimated_bytes - 1,
            })
        );
    }

    #[test]
    fn unfiltered_size_accounting_rejects_the_fixed_width_lower_bound_first() {
        let schema =
            crate::Schema::new(vec![ColumnSchema::new("payload", crate::DataType::String)])
                .unwrap();
        let mut table = Table::new(schema);
        table
            .insert_batch(vec![
                vec![Value::String("first payload".to_owned())],
                vec![Value::String("second payload".to_owned())],
            ])
            .unwrap();
        let columns = [ResolvedColumn {
            index: 0,
            schema: table.schema().column(0).unwrap(),
            values: table.column(0).unwrap(),
        }];
        let header_bytes = std::mem::size_of::<ColumnSchema>() + "payload".len();
        let fixed_bytes =
            table.row_count() * (std::mem::size_of::<Vec<Value>>() + std::mem::size_of::<Value>());
        let fixed_width_lower_bound = header_bytes + fixed_bytes;

        assert_eq!(
            enforce_result_size_limit(&table, &columns, None, fixed_width_lower_bound - 1),
            Err(TableSelectError::ResultSizeLimitExceeded {
                estimated_bytes: fixed_width_lower_bound,
                limit: fixed_width_lower_bound - 1,
            })
        );
    }

    #[test]
    fn filtered_size_accounting_stops_at_the_exceeded_row_lower_bound() {
        let schema = crate::Schema::new(vec![
            ColumnSchema::new("payload", crate::DataType::String),
            ColumnSchema::new("matches", crate::DataType::Bool),
        ])
        .unwrap();
        let mut table = Table::new(schema);
        table
            .insert_batch(vec![
                vec![Value::String("first".to_owned()), Value::Bool(true)],
                vec![Value::String("second".to_owned()), Value::Bool(true)],
            ])
            .unwrap();
        let columns = [ResolvedColumn {
            index: 0,
            schema: table.schema().column(0).unwrap(),
            values: table.column(0).unwrap(),
        }];
        let predicate = ResolvedPredicate {
            values: table.column(1).unwrap(),
            value: Value::Bool(true),
        };
        let header_bytes = std::mem::size_of::<ColumnSchema>() + "payload".len();
        let fixed_bytes_per_row = std::mem::size_of::<Vec<Value>>() + std::mem::size_of::<Value>();
        let first_row_bytes = header_bytes + fixed_bytes_per_row + "first".len();
        let exceeded_lower_bound = first_row_bytes + fixed_bytes_per_row;

        assert_eq!(
            enforce_result_size_limit(&table, &columns, Some(&predicate), first_row_bytes),
            Err(TableSelectError::ResultSizeLimitExceeded {
                estimated_bytes: exceeded_lower_bound,
                limit: first_row_bytes,
            })
        );
    }
}
