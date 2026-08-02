//! RustHouse is an experimental, compact analytical database.

mod database;
pub mod storage;

pub use database::{DEFAULT_TABLE_ROW_LIMIT, Database};
pub use storage::{
    AppendError, BatchAppendError, Column, DataType, Field, MAX_IDENTIFIER_BYTES,
    MAX_SCHEMA_FIELDS, MAX_STORED_STRING_BYTES, Schema, SchemaError, Table, TypedColumn,
    ValidityBitmap, Value, ValueType,
};

use std::borrow::Cow;
use std::fmt;
use std::io::{self, Write};

/// Maximum number of UTF-8 SQL bytes accepted by [`Database::execute`] and a
/// single CLI invocation. The limit is 32 MiB (33,554,432 bytes).
pub const MAX_SQL_INPUT_BYTES: usize = 32 * 1024 * 1024;

/// Maximum number of statements accepted in one SQL batch.
///
/// This separately bounds parser and result cardinality for dense inputs that
/// are well below [`MAX_SQL_INPUT_BYTES`].
pub const MAX_SQL_STATEMENTS: usize = 10_000;

/// Returns the product name.
pub fn product_name() -> &'static str {
    "RustHouse"
}

/// A scalar value supported by the initial query surface.
#[derive(Debug, Clone, PartialEq)]
pub enum ScalarValue {
    /// SQL NULL, including the result of a comparison with a NULL operand.
    Null,
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
            Self::Null => Cow::Borrowed("\\N"),
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

/// The typed cause of a SQL parsing or execution error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlErrorKind {
    /// A SQL batch exceeds [`MAX_SQL_INPUT_BYTES`].
    InputTooLarge {
        /// Maximum accepted batch size in UTF-8 bytes.
        max_bytes: usize,
    },
    /// A SQL batch contains more than [`MAX_SQL_STATEMENTS`] statements.
    TooManyStatements {
        /// Maximum accepted number of statements in one batch.
        max_statements: usize,
    },
    /// The input does not match the supported SQL grammar.
    Syntax {
        /// A concise description of what was expected or invalid.
        message: String,
    },
    /// A table name is already present in the catalog or batch.
    DuplicateTable {
        /// The repeated table name as written in the failing statement.
        table: String,
    },
    /// A table definition repeats a field name.
    DuplicateField {
        /// The table being defined.
        table: String,
        /// The repeated field name as written in the failing definition.
        field: String,
    },
    /// A field uses a type that the storage layer does not support.
    UnknownDataType {
        /// The unrecognized type name.
        data_type: String,
    },
    /// An INSERT names a table that is not present at that point in the batch.
    UnknownTable {
        /// The table name as written in the failing statement.
        table: String,
    },
    /// A schema-ordered INSERT row is invalid for its target table.
    InvalidRow {
        /// The target table name as written in the failing statement.
        table: String,
        /// The typed storage validation failure.
        source: BatchAppendError,
    },
    /// A `CREATE TABLE` definition violates a storage schema limit.
    InvalidSchema {
        /// The table whose schema is invalid.
        table: String,
        /// The typed storage-layer validation error.
        error: SchemaError,
    },
}

impl fmt::Display for SqlErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputTooLarge { max_bytes } => {
                write!(formatter, "SQL input exceeds the {max_bytes}-byte limit")
            }
            Self::TooManyStatements { max_statements } => write!(
                formatter,
                "SQL batch exceeds the {max_statements}-statement limit"
            ),
            Self::Syntax { message } => formatter.write_str(message),
            Self::DuplicateTable { table } => write!(formatter, "table `{table}` already exists"),
            Self::DuplicateField { table, field } => {
                write!(
                    formatter,
                    "table `{table}` contains duplicate field `{field}`"
                )
            }
            Self::UnknownDataType { data_type } => {
                write!(formatter, "unknown data type `{data_type}`")
            }
            Self::UnknownTable { table } => write!(formatter, "unknown table `{table}`"),
            Self::InvalidRow { table, source } => {
                write!(formatter, "cannot insert into table `{table}`: {source}")
            }
            Self::InvalidSchema { table, error } => {
                write!(formatter, "invalid schema for table `{table}`: {error}")
            }
        }
    }
}

/// A typed, position-aware error in a SQL batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlError {
    byte_offset: usize,
    line: usize,
    column: usize,
    kind: SqlErrorKind,
}

impl SqlError {
    /// Returns the zero-based UTF-8 byte offset of the failing token.
    pub fn byte_offset(&self) -> usize {
        self.byte_offset
    }

    /// Returns the one-based source line of the failing token.
    pub fn line(&self) -> usize {
        self.line
    }

    /// Returns the one-based character column of the failing token.
    pub fn column(&self) -> usize {
        self.column
    }

    /// Returns the typed cause of the error.
    pub fn kind(&self) -> &SqlErrorKind {
        &self.kind
    }

    fn at(input: &str, byte_offset: usize, kind: SqlErrorKind) -> Self {
        let (line, column) = line_and_column(input, byte_offset);
        Self {
            byte_offset,
            line,
            column,
            kind,
        }
    }
}

impl fmt::Display for SqlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "SQL error at line {}, column {}: {}",
            self.line, self.column, self.kind
        )
    }
}

impl std::error::Error for SqlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            SqlErrorKind::InvalidRow { source, .. } => Some(source),
            SqlErrorKind::InvalidSchema { error, .. } => Some(error),
            _ => None,
        }
    }
}

/// Parses a batch of scalar `SELECT` statements.
///
/// An expression is either a literal or one `=` or `<>` comparison between
/// literals. Comparisons use SQL NULL propagation and require non-null
/// operands to have the same type.
pub fn parse_sql_batch(input: &str) -> Result<Vec<QueryResult>, SqlError> {
    Parser::new(input, ParserMode::SelectOnly)
        .parse_batch()?
        .into_iter()
        .map(|statement| match statement {
            Statement::Select(result) => Ok(result),
            Statement::CreateTable(_) => unreachable!("SELECT-only parsing produced CREATE TABLE"),
            Statement::InsertInto(_) => unreachable!("SELECT-only parsing produced INSERT"),
        })
        .collect()
}

#[derive(Clone, Copy)]
enum ParserMode {
    SelectOnly,
    Database,
}

#[derive(Debug)]
pub(crate) enum Statement {
    Select(QueryResult),
    CreateTable(CreateTable),
    InsertInto(InsertInto),
}

#[derive(Debug)]
pub(crate) struct CreateTable {
    pub(crate) name: PositionedIdentifier,
    pub(crate) fields: Vec<CreateField>,
}

#[derive(Debug)]
pub(crate) struct CreateField {
    pub(crate) name: PositionedIdentifier,
    pub(crate) data_type: DataType,
}

#[derive(Debug)]
pub(crate) struct InsertInto {
    pub(crate) table: PositionedIdentifier,
    pub(crate) rows: Vec<InsertRow>,
}

#[derive(Debug)]
pub(crate) struct InsertRow {
    pub(crate) byte_offset: usize,
    pub(crate) values: Vec<Value>,
}

#[derive(Debug)]
pub(crate) struct PositionedIdentifier {
    pub(crate) value: String,
    pub(crate) byte_offset: usize,
}

pub(crate) fn parse_database_batch(input: &str) -> Result<Vec<Statement>, SqlError> {
    Parser::new(input, ParserMode::Database).parse_batch()
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
    mode: ParserMode,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str, mode: ParserMode) -> Self {
        Self {
            input,
            position: 0,
            mode,
        }
    }

    fn parse_batch(mut self) -> Result<Vec<Statement>, SqlError> {
        let mut statements = Vec::new();
        self.skip_whitespace();

        if self.is_at_end() {
            return Err(self.syntax_error(match self.mode {
                ParserMode::SelectOnly => "expected a SELECT statement",
                ParserMode::Database => "expected a SELECT, CREATE TABLE, or INSERT statement",
            }));
        }

        while !self.is_at_end() {
            if statements.len() == MAX_SQL_STATEMENTS {
                return Err(SqlError::at(
                    self.input,
                    self.position,
                    SqlErrorKind::TooManyStatements {
                        max_statements: MAX_SQL_STATEMENTS,
                    },
                ));
            }
            statements.push(self.parse_statement()?);
            self.skip_whitespace();
        }

        Ok(statements)
    }

    fn parse_statement(&mut self) -> Result<Statement, SqlError> {
        if self.consume_keyword("SELECT") {
            return self.parse_select().map(Statement::Select);
        }
        if matches!(self.mode, ParserMode::Database) && self.consume_keyword("CREATE") {
            return self.parse_create_table().map(Statement::CreateTable);
        }
        if matches!(self.mode, ParserMode::Database) && self.consume_keyword("INSERT") {
            return self.parse_insert_into().map(Statement::InsertInto);
        }

        Err(self.syntax_error(match self.mode {
            ParserMode::SelectOnly => "expected SELECT",
            ParserMode::Database => "expected SELECT, CREATE TABLE, or INSERT",
        }))
    }

    fn parse_select(&mut self) -> Result<QueryResult, SqlError> {
        self.skip_whitespace();

        let expression_start = self.position;
        let value = self.parse_expression()?;
        let expression_end = self.position;
        self.skip_whitespace();
        let has_alias_separator = self.position > expression_end;

        let header = if self.consume_keyword("AS") {
            if !has_alias_separator {
                return Err(self.syntax_error_at(expression_end, "expected whitespace before AS"));
            }
            self.skip_whitespace();
            self.parse_identifier("expected an identifier after AS")?
                .value
        } else {
            self.input[expression_start..expression_end].to_owned()
        };

        self.skip_whitespace();
        if self.peek() != Some(';') {
            return Err(self.syntax_error("expected ';' after SELECT statement"));
        }
        self.advance();

        Ok(QueryResult { header, value })
    }

    fn parse_create_table(&mut self) -> Result<CreateTable, SqlError> {
        if !self.skip_required_whitespace() || !self.consume_keyword("TABLE") {
            return Err(self.syntax_error("expected TABLE after CREATE"));
        }
        if !self.skip_required_whitespace() {
            return Err(self.syntax_error("expected a table name after CREATE TABLE"));
        }

        let name = self.parse_identifier("expected a table name after CREATE TABLE")?;
        self.skip_whitespace();
        if self.peek() != Some('(') {
            return Err(self.syntax_error("expected '(' after table name"));
        }
        self.advance();
        self.skip_whitespace();

        if self.peek() == Some(')') {
            return Err(self.syntax_error("expected at least one field definition"));
        }

        let mut fields = Vec::new();
        let mut field_names = std::collections::HashSet::new();
        loop {
            let field_name = self.parse_identifier("expected a field name")?;
            if !field_names.insert(field_name.value.to_ascii_lowercase()) {
                return Err(SqlError::at(
                    self.input,
                    field_name.byte_offset,
                    SqlErrorKind::DuplicateField {
                        table: name.value.clone(),
                        field: field_name.value,
                    },
                ));
            }
            if !self.skip_required_whitespace() {
                return Err(self.syntax_error("expected a data type after field name"));
            }

            let type_name = self.parse_identifier("expected a data type after field name")?;
            let data_type = match type_name.value.to_ascii_lowercase().as_str() {
                "int64" => DataType::Int64,
                "float64" => DataType::Float64,
                "bool" => DataType::Bool,
                "string" => DataType::String,
                _ => {
                    return Err(SqlError::at(
                        self.input,
                        type_name.byte_offset,
                        SqlErrorKind::UnknownDataType {
                            data_type: type_name.value,
                        },
                    ));
                }
            };
            fields.push(CreateField {
                name: field_name,
                data_type,
            });

            self.skip_whitespace();
            match self.peek() {
                Some(',') => {
                    self.advance();
                    self.skip_whitespace();
                }
                Some(')') => {
                    self.advance();
                    break;
                }
                _ => return Err(self.syntax_error("expected ',' or ')' after field definition")),
            }
        }

        self.skip_whitespace();
        if self.peek() != Some(';') {
            return Err(self.syntax_error("expected ';' after CREATE TABLE statement"));
        }
        self.advance();

        Ok(CreateTable { name, fields })
    }

    fn parse_insert_into(&mut self) -> Result<InsertInto, SqlError> {
        if !self.skip_required_whitespace() || !self.consume_keyword("INTO") {
            return Err(self.syntax_error("expected INTO after INSERT"));
        }
        if !self.skip_required_whitespace() {
            return Err(self.syntax_error("expected a table name after INSERT INTO"));
        }

        let table = self.parse_identifier("expected a table name after INSERT INTO")?;
        if !self.skip_required_whitespace() || !self.consume_keyword("VALUES") {
            return Err(self.syntax_error("expected VALUES after table name"));
        }
        self.skip_whitespace();

        let mut rows = Vec::new();
        loop {
            let byte_offset = self.position;
            if self.peek() != Some('(') {
                return Err(self.syntax_error("expected '(' to start an INSERT row"));
            }
            self.advance();
            self.skip_whitespace();

            let mut values = Vec::new();
            if self.peek() != Some(')') {
                loop {
                    values.push(self.parse_insert_literal()?);
                    self.skip_whitespace();
                    match self.peek() {
                        Some(',') => {
                            self.advance();
                            self.skip_whitespace();
                        }
                        Some(')') => break,
                        _ => {
                            return Err(self.syntax_error("expected ',' or ')' after INSERT value"));
                        }
                    }
                }
            }

            self.advance();
            rows.push(InsertRow {
                byte_offset,
                values,
            });
            self.skip_whitespace();

            match self.peek() {
                Some(',') => {
                    self.advance();
                    self.skip_whitespace();
                }
                Some(';') => {
                    self.advance();
                    break;
                }
                _ => return Err(self.syntax_error("expected ',' or ';' after INSERT row")),
            }
        }

        Ok(InsertInto { table, rows })
    }

    fn parse_insert_literal(&mut self) -> Result<Value, SqlError> {
        Ok(match self.parse_literal()? {
            ScalarValue::Null => Value::Null,
            ScalarValue::Integer(value) => Value::Int64(value),
            ScalarValue::Float(value) => Value::Float64(value),
            ScalarValue::Boolean(value) => Value::Bool(value),
            ScalarValue::String(value) => Value::String(value),
        })
    }

    fn parse_expression(&mut self) -> Result<ScalarValue, SqlError> {
        let left = self.parse_literal()?;
        let left_end = self.position;
        self.skip_whitespace();

        let operator_position = self.position;
        let operator = if self.input[self.position..].starts_with("<>") {
            self.position += 2;
            Some(ComparisonOperator::NotEqual)
        } else if self.peek() == Some('=') {
            self.advance();
            Some(ComparisonOperator::Equal)
        } else {
            None
        };

        let Some(operator) = operator else {
            self.position = left_end;
            return Ok(left);
        };

        self.skip_whitespace();
        let right = self.parse_literal()?;
        compare_scalars(left, right, operator)
            .map_err(|message| self.syntax_error_at(operator_position, message))
    }

    fn parse_literal(&mut self) -> Result<ScalarValue, SqlError> {
        match self.peek() {
            Some('\'') => self.parse_string().map(ScalarValue::String),
            Some('+') | Some('-') | Some('.') | Some('0'..='9') => self.parse_number(),
            _ if self.consume_keyword("NULL") => Ok(ScalarValue::Null),
            _ if self.consume_keyword("TRUE") => Ok(ScalarValue::Boolean(true)),
            _ if self.consume_keyword("FALSE") => Ok(ScalarValue::Boolean(false)),
            _ => Err(self.syntax_error(
                "expected NULL, an integer, finite float, boolean, or quoted string literal",
            )),
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
                None => return Err(self.syntax_error_at(start, "unterminated quoted string")),
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
            return Err(self.syntax_error_at(start, "invalid numeric literal"));
        }

        let has_exponent = if matches!(self.peek(), Some('e') | Some('E')) {
            let exponent_start = self.position;
            self.advance();
            if matches!(self.peek(), Some('+') | Some('-')) {
                self.advance();
            }
            if self.consume_digits() == 0 {
                return Err(self.syntax_error_at(exponent_start, "invalid float exponent"));
            }
            true
        } else {
            false
        };

        let literal = &self.input[start..self.position];
        if has_decimal || has_exponent {
            let value = literal
                .parse::<f64>()
                .map_err(|_| self.syntax_error_at(start, "invalid float literal"))?;
            if !value.is_finite() {
                return Err(self.syntax_error_at(start, "float literal must be finite"));
            }
            Ok(ScalarValue::Float(value))
        } else {
            literal
                .parse::<i64>()
                .map(ScalarValue::Integer)
                .map_err(|_| {
                    self.syntax_error_at(start, "integer literal is outside the Int64 range")
                })
        }
    }

    fn parse_identifier(
        &mut self,
        expected_message: &'static str,
    ) -> Result<PositionedIdentifier, SqlError> {
        let start = self.position;
        match self.peek() {
            Some(character) if character.is_ascii_alphabetic() || character == '_' => {
                self.advance();
            }
            _ => return Err(self.syntax_error(expected_message)),
        }

        while matches!(self.peek(), Some(character) if character.is_ascii_alphanumeric() || character == '_')
        {
            self.advance();
        }

        Ok(PositionedIdentifier {
            value: self.input[start..self.position].to_owned(),
            byte_offset: start,
        })
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

    fn skip_required_whitespace(&mut self) -> bool {
        let start = self.position;
        self.skip_whitespace();
        self.position > start
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

    fn syntax_error(&self, message: impl Into<String>) -> SqlError {
        self.syntax_error_at(self.position, message)
    }

    fn syntax_error_at(&self, position: usize, message: impl Into<String>) -> SqlError {
        SqlError::at(
            self.input,
            position,
            SqlErrorKind::Syntax {
                message: message.into(),
            },
        )
    }
}

#[derive(Clone, Copy)]
enum ComparisonOperator {
    Equal,
    NotEqual,
}

impl ComparisonOperator {
    fn sql(self) -> &'static str {
        match self {
            Self::Equal => "=",
            Self::NotEqual => "<>",
        }
    }

    fn apply(self, equal: bool) -> bool {
        match self {
            Self::Equal => equal,
            Self::NotEqual => !equal,
        }
    }
}

fn compare_scalars(
    left: ScalarValue,
    right: ScalarValue,
    operator: ComparisonOperator,
) -> Result<ScalarValue, String> {
    let equal = match (left, right) {
        (ScalarValue::Null, _) | (_, ScalarValue::Null) => return Ok(ScalarValue::Null),
        (ScalarValue::Integer(left), ScalarValue::Integer(right)) => left == right,
        (ScalarValue::Float(left), ScalarValue::Float(right)) => left == right,
        (ScalarValue::Boolean(left), ScalarValue::Boolean(right)) => left == right,
        (ScalarValue::String(left), ScalarValue::String(right)) => left == right,
        (left, right) => {
            return Err(format!(
                "operator '{}' cannot compare {} and {}",
                operator.sql(),
                scalar_type_name(&left),
                scalar_type_name(&right)
            ));
        }
    };

    Ok(ScalarValue::Boolean(operator.apply(equal)))
}

fn scalar_type_name(value: &ScalarValue) -> &'static str {
    match value {
        ScalarValue::Null => "NULL",
        ScalarValue::Integer(_) => "Integer",
        ScalarValue::Float(_) => "Float",
        ScalarValue::Boolean(_) => "Boolean",
        ScalarValue::String(_) => "String",
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
             SELECT FALSE; SELECT 'it''s text' AS string_value; SELECT NULL;",
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
        assert_eq!(results[4].header, "NULL");
        assert_eq!(results[4].value, ScalarValue::Null);
    }

    #[test]
    fn evaluates_comparison_truth_tables() {
        let results = parse_sql_batch(
            "SELECT 2 = 2; SELECT 2 <> 2; SELECT 2 = 3; SELECT 2 <> 3; \
             SELECT 1.5 = 1.5; SELECT 1.5 <> 2.5; \
             SELECT TRUE = TRUE; SELECT TRUE <> FALSE; \
             SELECT 'same' = 'same'; SELECT 'same' <> 'other'; \
             SELECT NULL = NULL; SELECT NULL <> 1; SELECT 'text' = NULL;",
        )
        .unwrap();

        assert_eq!(
            results
                .iter()
                .map(|result| &result.value)
                .collect::<Vec<_>>(),
            vec![
                &ScalarValue::Boolean(true),
                &ScalarValue::Boolean(false),
                &ScalarValue::Boolean(false),
                &ScalarValue::Boolean(true),
                &ScalarValue::Boolean(true),
                &ScalarValue::Boolean(true),
                &ScalarValue::Boolean(true),
                &ScalarValue::Boolean(true),
                &ScalarValue::Boolean(true),
                &ScalarValue::Boolean(true),
                &ScalarValue::Null,
                &ScalarValue::Null,
                &ScalarValue::Null,
            ]
        );
        assert_eq!(results[0].header, "2 = 2");
    }

    #[test]
    fn reports_mixed_comparison_types_at_the_operator() {
        for (sql, operator, left_type, right_type) in [
            ("SELECT 1 = 1.0;", "=", "Integer", "Float"),
            ("SELECT FALSE <> 'false';", "<>", "Boolean", "String"),
        ] {
            let error = parse_sql_batch(sql).unwrap_err();

            assert_eq!(error.line(), 1);
            assert_eq!(error.column(), sql.find(operator).unwrap() + 1);
            assert_eq!(
                error.kind(),
                &SqlErrorKind::Syntax {
                    message: format!(
                        "operator '{operator}' cannot compare {left_type} and {right_type}"
                    ),
                }
            );
        }
    }

    #[test]
    fn accepts_one_compact_comparison_and_rejects_a_second_operator() {
        let result = parse_sql_batch("SELECT 1=1;").unwrap();
        assert_eq!(result[0].header, "1=1");
        assert_eq!(result[0].value, ScalarValue::Boolean(true));

        let sql = "SELECT 1 = 1 <> 2;";
        let error = parse_sql_batch(sql).unwrap_err();
        assert_eq!(error.line(), 1);
        assert_eq!(error.column(), sql.find("<>").unwrap() + 1);
        assert_eq!(
            error.kind(),
            &SqlErrorKind::Syntax {
                message: "expected ';' after SELECT statement".into(),
            }
        );
    }

    #[test]
    fn quotes_csv_fields() {
        let results = vec![
            QueryResult {
                header: "message, \"text\"".to_owned(),
                value: ScalarValue::String("one, \"two\"\nthree".to_owned()),
            },
            QueryResult {
                header: "missing".to_owned(),
                value: ScalarValue::Null,
            },
        ];
        let mut output = Vec::new();

        write_csv(&results, &mut output).unwrap();

        assert_eq!(
            String::from_utf8(output).unwrap(),
            "\"message, \"\"text\"\"\"\n\"one, \"\"two\"\"\nthree\"\nmissing\n\\N\n"
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
