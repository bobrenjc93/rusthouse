//! Parsing and syntax tree types for RustHouse's initial SQL boundary.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;

pub use crate::storage::{DataType, Value};

/// One named, typed column in a table declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDefinition {
    pub name: String,
    pub data_type: DataType,
}

/// The syntax tree produced for a `CREATE TABLE` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateTableStatement {
    pub name: String,
    pub columns: Vec<ColumnDefinition>,
}

/// The syntax tree produced for an `INSERT INTO ... VALUES` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct InsertStatement {
    pub name: String,
    pub rows: Vec<Vec<Value>>,
}

/// The columns requested by a `SELECT` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectProjection {
    /// Expand every column in the table at planning time.
    All,
    /// Return the named columns in the specified order.
    Columns(Vec<String>),
}

/// A comparison relationship supported by a `WHERE` predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComparisonOperator {
    Equal,
    NotEqual,
    LessThan,
    LessThanOrEqual,
    GreaterThan,
    GreaterThanOrEqual,
}

/// One column-to-literal comparison in a `WHERE` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct ComparisonPredicate {
    pub column: String,
    pub operator: ComparisonOperator,
    pub value: Value,
}

/// The syntax tree produced for a bounded `SELECT` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectStatement {
    pub projections: SelectProjection,
    pub table: String,
    pub predicate: Option<ComparisonPredicate>,
    pub limit: Option<usize>,
}

/// Resource limits applied before and during parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseLimits {
    pub max_input_bytes: usize,
    pub max_columns: usize,
}

impl ParseLimits {
    pub const DEFAULT_MAX_INPUT_BYTES: usize = 1024 * 1024;
    pub const DEFAULT_MAX_COLUMNS: usize = 1024;

    pub const fn new(max_input_bytes: usize, max_columns: usize) -> Self {
        Self {
            max_input_bytes,
            max_columns,
        }
    }
}

impl Default for ParseLimits {
    fn default() -> Self {
        Self::new(Self::DEFAULT_MAX_INPUT_BYTES, Self::DEFAULT_MAX_COLUMNS)
    }
}

/// Resource limits applied before and during `INSERT` parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InsertParseLimits {
    pub max_input_bytes: usize,
    pub max_rows: usize,
    pub max_values_per_row: usize,
    pub max_string_bytes: usize,
}

impl InsertParseLimits {
    pub const DEFAULT_MAX_INPUT_BYTES: usize = 1024 * 1024;
    pub const DEFAULT_MAX_ROWS: usize = 100_000;
    pub const DEFAULT_MAX_VALUES_PER_ROW: usize = 1024;
    pub const DEFAULT_MAX_STRING_BYTES: usize = 1024 * 1024;

    pub const fn new(
        max_input_bytes: usize,
        max_rows: usize,
        max_values_per_row: usize,
        max_string_bytes: usize,
    ) -> Self {
        Self {
            max_input_bytes,
            max_rows,
            max_values_per_row,
            max_string_bytes,
        }
    }
}

impl Default for InsertParseLimits {
    fn default() -> Self {
        Self::new(
            Self::DEFAULT_MAX_INPUT_BYTES,
            Self::DEFAULT_MAX_ROWS,
            Self::DEFAULT_MAX_VALUES_PER_ROW,
            Self::DEFAULT_MAX_STRING_BYTES,
        )
    }
}

/// Resource limits applied before and during `SELECT` parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectParseLimits {
    pub max_input_bytes: usize,
    pub max_projections: usize,
}

impl SelectParseLimits {
    pub const DEFAULT_MAX_INPUT_BYTES: usize = 1024 * 1024;
    pub const DEFAULT_MAX_PROJECTIONS: usize = 1024;

    pub const fn new(max_input_bytes: usize, max_projections: usize) -> Self {
        Self {
            max_input_bytes,
            max_projections,
        }
    }
}

impl Default for SelectParseLimits {
    fn default() -> Self {
        Self::new(Self::DEFAULT_MAX_INPUT_BYTES, Self::DEFAULT_MAX_PROJECTIONS)
    }
}

/// The role of an identifier which failed validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentifierContext {
    Table,
    Column,
}

impl fmt::Display for IdentifierContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Table => formatter.write_str("table"),
            Self::Column => formatter.write_str("column"),
        }
    }
}

/// A specific reason that a supported SQL statement could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseErrorKind {
    InputTooLong {
        limit: usize,
        actual: usize,
    },
    ExpectedKeyword {
        expected: &'static str,
        found: Option<String>,
    },
    ExpectedIdentifier {
        context: IdentifierContext,
    },
    InvalidIdentifier {
        context: IdentifierContext,
        identifier: String,
    },
    ExpectedToken {
        expected: &'static str,
    },
    EmptyColumn,
    DuplicateColumn {
        name: String,
        first_position: usize,
    },
    ExpectedType,
    UnknownType {
        type_name: String,
    },
    TooManyColumns {
        limit: usize,
    },
    TooManyRows {
        limit: usize,
    },
    EmptyRow,
    TooManyValues {
        limit: usize,
    },
    ExpectedProjection,
    TooManyProjections {
        limit: usize,
    },
    ExpectedComparisonOperator,
    InvalidComparisonOperator {
        operator: String,
    },
    ExpectedLimit,
    InvalidLimit {
        literal: String,
    },
    LimitOutOfRange {
        literal: String,
    },
    ExpectedValue,
    InvalidLiteral {
        literal: String,
    },
    IntegerLiteralOutOfRange {
        literal: String,
    },
    FloatLiteralOutOfRange {
        literal: String,
    },
    UnterminatedString,
    StringTooLong {
        limit: usize,
    },
    TrailingSyntax,
}

impl fmt::Display for ParseErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputTooLong { limit, actual } => {
                write!(formatter, "input is {actual} bytes; limit is {limit}")
            }
            Self::ExpectedKeyword { expected, found } => match found {
                Some(found) => write!(formatter, "expected keyword {expected}, found {found:?}"),
                None => write!(formatter, "expected keyword {expected}"),
            },
            Self::ExpectedIdentifier { context } => {
                write!(formatter, "expected {context} identifier")
            }
            Self::InvalidIdentifier {
                context,
                identifier,
            } => write!(formatter, "invalid {context} identifier {identifier:?}"),
            Self::ExpectedToken { expected } => write!(formatter, "expected {expected}"),
            Self::EmptyColumn => formatter.write_str("column declaration is empty"),
            Self::DuplicateColumn {
                name,
                first_position,
            } => write!(
                formatter,
                "duplicate column {name:?}; first declared at byte {first_position}"
            ),
            Self::ExpectedType => formatter.write_str("expected column type"),
            Self::UnknownType { type_name } => {
                write!(formatter, "unknown column type {type_name:?}")
            }
            Self::TooManyColumns { limit } => {
                write!(formatter, "column count exceeds limit of {limit}")
            }
            Self::TooManyRows { limit } => {
                write!(formatter, "row count exceeds limit of {limit}")
            }
            Self::EmptyRow => formatter.write_str("row contains no values"),
            Self::TooManyValues { limit } => {
                write!(formatter, "row value count exceeds limit of {limit}")
            }
            Self::ExpectedProjection => formatter.write_str("expected a column projection or '*'"),
            Self::TooManyProjections { limit } => {
                write!(formatter, "projection count exceeds limit of {limit}")
            }
            Self::ExpectedComparisonOperator => {
                formatter.write_str("expected a comparison operator")
            }
            Self::InvalidComparisonOperator { operator } => {
                write!(formatter, "invalid comparison operator {operator:?}")
            }
            Self::ExpectedLimit => formatter.write_str("expected a nonnegative integer limit"),
            Self::InvalidLimit { literal } => {
                write!(
                    formatter,
                    "invalid limit {literal:?}; expected a nonnegative integer"
                )
            }
            Self::LimitOutOfRange { literal } => {
                write!(
                    formatter,
                    "limit {literal:?} is outside the supported range"
                )
            }
            Self::ExpectedValue => formatter.write_str("expected a literal value"),
            Self::InvalidLiteral { literal } => {
                write!(formatter, "invalid literal {literal:?}")
            }
            Self::IntegerLiteralOutOfRange { literal } => {
                write!(
                    formatter,
                    "integer literal {literal:?} is outside the Int64 range"
                )
            }
            Self::FloatLiteralOutOfRange { literal } => {
                write!(
                    formatter,
                    "float literal {literal:?} is outside the Float64 range"
                )
            }
            Self::UnterminatedString => formatter.write_str("unterminated string literal"),
            Self::StringTooLong { limit } => {
                write!(formatter, "decoded string exceeds limit of {limit} bytes")
            }
            Self::TrailingSyntax => formatter.write_str("trailing syntax after statement"),
        }
    }
}

/// A parse error and the zero-based byte position at which it was detected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub position: usize,
    pub kind: ParseErrorKind,
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "SQL parse error at byte {}: {}",
            self.position, self.kind
        )
    }
}

impl Error for ParseError {}

/// Parses one bounded `CREATE TABLE` statement using the default limits.
pub fn parse_create_table(input: &str) -> Result<CreateTableStatement, ParseError> {
    parse_create_table_with_limits(input, ParseLimits::default())
}

/// Parses one bounded `CREATE TABLE` statement.
///
/// Keywords and data types are case-insensitive. Unquoted identifiers must match
/// `[A-Za-z_][A-Za-z0-9_]*`. One trailing semicolon is accepted as a statement
/// terminator, but comments, quoted identifiers, and additional statements are
/// outside this parser's intentionally narrow SQL surface.
pub fn parse_create_table_with_limits(
    input: &str,
    limits: ParseLimits,
) -> Result<CreateTableStatement, ParseError> {
    if input.len() > limits.max_input_bytes {
        return Err(ParseError {
            position: limits.max_input_bytes,
            kind: ParseErrorKind::InputTooLong {
                limit: limits.max_input_bytes,
                actual: input.len(),
            },
        });
    }

    Parser::new(input).parse_create_table(limits.max_columns)
}

/// Parses one bounded `INSERT INTO ... VALUES` statement using the default limits.
pub fn parse_insert(input: &str) -> Result<InsertStatement, ParseError> {
    parse_insert_with_limits(input, InsertParseLimits::default())
}

/// Parses one bounded `INSERT INTO ... VALUES` statement.
///
/// The parser accepts one or more non-empty rows containing `Int64`, `Float64`,
/// `Bool`, and single-quoted `String` literals. A quote inside a string is
/// escaped by doubling it (`'can''t'`). String limits apply to decoded UTF-8
/// bytes, after doubled quotes have been reduced to one byte. Catalog lookup,
/// schema validation, and table mutation are deliberately outside this syntax
/// boundary.
pub fn parse_insert_with_limits(
    input: &str,
    limits: InsertParseLimits,
) -> Result<InsertStatement, ParseError> {
    if input.len() > limits.max_input_bytes {
        return Err(ParseError {
            position: limits.max_input_bytes,
            kind: ParseErrorKind::InputTooLong {
                limit: limits.max_input_bytes,
                actual: input.len(),
            },
        });
    }

    Parser::new(input).parse_insert(limits)
}

/// Parses one bounded `SELECT` statement using the default limits.
pub fn parse_select(input: &str) -> Result<SelectStatement, ParseError> {
    parse_select_with_limits(input, SelectParseLimits::default())
}

/// Parses one bounded `SELECT` statement.
///
/// Projections are either `*` or a non-empty list of unquoted column names.
/// The statement reads one table and may contain one `WHERE` comparison between
/// a column and an `Int64`, `Float64`, `Bool`, or `String` literal, followed by
/// an optional nonnegative integer `LIMIT`. Aliases, expressions, aggregates,
/// compound predicates, and other result modifiers are outside this
/// intentionally narrow syntax boundary.
pub fn parse_select_with_limits(
    input: &str,
    limits: SelectParseLimits,
) -> Result<SelectStatement, ParseError> {
    if input.len() > limits.max_input_bytes {
        return Err(ParseError {
            position: limits.max_input_bytes,
            kind: ParseErrorKind::InputTooLong {
                limit: limits.max_input_bytes,
                actual: input.len(),
            },
        });
    }

    Parser::new(input).parse_select(limits)
}

struct Parser<'a> {
    input: &'a str,
    position: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, position: 0 }
    }

    fn parse_create_table(
        mut self,
        max_columns: usize,
    ) -> Result<CreateTableStatement, ParseError> {
        self.parse_keyword("CREATE")?;
        self.parse_keyword("TABLE")?;
        let (name, _) = self.parse_identifier(IdentifierContext::Table)?;
        self.expect_byte(b'(', "'('")?;

        let mut columns = Vec::new();
        let mut column_positions = HashMap::new();

        loop {
            self.skip_whitespace();
            if self.peek().is_none() || matches!(self.peek(), Some(b')' | b',')) {
                return Err(self.error(ParseErrorKind::EmptyColumn));
            }
            if columns.len() == max_columns {
                return Err(self.error(ParseErrorKind::TooManyColumns { limit: max_columns }));
            }

            let (column_name, column_position) =
                self.parse_identifier(IdentifierContext::Column)?;
            let normalized_name = column_name.to_ascii_lowercase();
            if let Some(first_position) = column_positions.get(&normalized_name) {
                return Err(ParseError {
                    position: column_position,
                    kind: ParseErrorKind::DuplicateColumn {
                        name: column_name,
                        first_position: *first_position,
                    },
                });
            }

            let data_type = self.parse_data_type()?;
            column_positions.insert(normalized_name, column_position);
            columns.push(ColumnDefinition {
                name: column_name,
                data_type,
            });

            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.position += 1,
                Some(b')') => {
                    self.position += 1;
                    break;
                }
                _ => {
                    return Err(self.error(ParseErrorKind::ExpectedToken {
                        expected: "',' or ')'",
                    }));
                }
            }
        }

        self.skip_whitespace();
        if self.peek() == Some(b';') {
            self.position += 1;
            self.skip_whitespace();
        }
        if self.peek().is_some() {
            return Err(self.error(ParseErrorKind::TrailingSyntax));
        }

        Ok(CreateTableStatement { name, columns })
    }

    fn parse_insert(mut self, limits: InsertParseLimits) -> Result<InsertStatement, ParseError> {
        self.parse_keyword("INSERT")?;
        self.parse_keyword("INTO")?;
        let (name, _) = self.parse_identifier(IdentifierContext::Table)?;
        self.parse_keyword("VALUES")?;

        let mut rows = Vec::new();
        loop {
            self.skip_whitespace();
            if self.peek() != Some(b'(') {
                return Err(self.error(ParseErrorKind::ExpectedToken { expected: "'('" }));
            }
            if rows.len() == limits.max_rows {
                return Err(self.error(ParseErrorKind::TooManyRows {
                    limit: limits.max_rows,
                }));
            }
            self.position += 1;

            let row = self.parse_row(limits)?;
            rows.push(row);

            self.skip_whitespace();
            if self.peek() == Some(b',') {
                self.position += 1;
                continue;
            }
            break;
        }

        self.finish_statement()?;
        Ok(InsertStatement { name, rows })
    }

    fn parse_select(mut self, limits: SelectParseLimits) -> Result<SelectStatement, ParseError> {
        self.parse_keyword("SELECT")?;
        let projections = self.parse_projections(limits.max_projections)?;
        self.parse_keyword("FROM")?;
        let (table, _) = self.parse_identifier(IdentifierContext::Table)?;

        self.skip_whitespace();
        let predicate = if self.peek_token_is("WHERE") {
            self.parse_keyword("WHERE")?;
            Some(self.parse_comparison(limits.max_input_bytes)?)
        } else {
            None
        };

        self.skip_whitespace();
        let limit = if self.peek_token_is("LIMIT") {
            self.parse_keyword("LIMIT")?;
            Some(self.parse_limit()?)
        } else {
            None
        };

        self.finish_statement()?;
        Ok(SelectStatement {
            projections,
            table,
            predicate,
            limit,
        })
    }

    fn parse_projections(
        &mut self,
        max_projections: usize,
    ) -> Result<SelectProjection, ParseError> {
        self.skip_whitespace();
        if self.peek() == Some(b'*') {
            if max_projections == 0 {
                return Err(self.error(ParseErrorKind::TooManyProjections {
                    limit: max_projections,
                }));
            }
            self.position += 1;
            return Ok(SelectProjection::All);
        }

        let mut columns = Vec::new();
        loop {
            self.skip_whitespace();
            if self.peek().is_none() || self.peek() == Some(b',') || self.peek_token_is("FROM") {
                return Err(self.error(ParseErrorKind::ExpectedProjection));
            }
            if columns.len() == max_projections {
                return Err(self.error(ParseErrorKind::TooManyProjections {
                    limit: max_projections,
                }));
            }

            let (column, _) = self.parse_identifier(IdentifierContext::Column)?;
            columns.push(column);

            self.skip_whitespace();
            if self.peek() != Some(b',') {
                break;
            }
            self.position += 1;
        }

        Ok(SelectProjection::Columns(columns))
    }

    fn parse_comparison(
        &mut self,
        max_string_bytes: usize,
    ) -> Result<ComparisonPredicate, ParseError> {
        let column = self.parse_comparison_column()?;
        let operator = self.parse_comparison_operator()?;
        let value = self.parse_value(max_string_bytes)?;
        Ok(ComparisonPredicate {
            column,
            operator,
            value,
        })
    }

    fn parse_comparison_column(&mut self) -> Result<String, ParseError> {
        self.skip_whitespace();
        let start = self.position;
        while let Some(byte) = self.peek() {
            if is_whitespace(byte)
                || matches!(byte, b'(' | b')' | b',' | b';' | b'=' | b'!' | b'<' | b'>')
            {
                break;
            }
            self.position += 1;
        }

        let identifier = &self.input[start..self.position];
        if identifier.is_empty() {
            return Err(ParseError {
                position: start,
                kind: ParseErrorKind::ExpectedIdentifier {
                    context: IdentifierContext::Column,
                },
            });
        }
        if let Some(offset) = invalid_identifier_offset(identifier) {
            return Err(ParseError {
                position: start + offset,
                kind: ParseErrorKind::InvalidIdentifier {
                    context: IdentifierContext::Column,
                    identifier: identifier.to_owned(),
                },
            });
        }

        Ok(identifier.to_owned())
    }

    fn parse_comparison_operator(&mut self) -> Result<ComparisonOperator, ParseError> {
        self.skip_whitespace();
        let start = self.position;
        while self
            .peek()
            .is_some_and(|byte| matches!(byte, b'=' | b'!' | b'<' | b'>'))
        {
            self.position += 1;
        }

        let operator = &self.input[start..self.position];
        match operator {
            "=" => Ok(ComparisonOperator::Equal),
            "!=" | "<>" => Ok(ComparisonOperator::NotEqual),
            "<" => Ok(ComparisonOperator::LessThan),
            "<=" => Ok(ComparisonOperator::LessThanOrEqual),
            ">" => Ok(ComparisonOperator::GreaterThan),
            ">=" => Ok(ComparisonOperator::GreaterThanOrEqual),
            "" => Err(ParseError {
                position: start,
                kind: ParseErrorKind::ExpectedComparisonOperator,
            }),
            _ => Err(ParseError {
                position: start,
                kind: ParseErrorKind::InvalidComparisonOperator {
                    operator: operator.to_owned(),
                },
            }),
        }
    }

    fn parse_limit(&mut self) -> Result<usize, ParseError> {
        self.skip_whitespace();
        let start = self.position;
        let literal = self.take_token();
        if literal.is_empty() {
            return Err(ParseError {
                position: start,
                kind: ParseErrorKind::ExpectedLimit,
            });
        }
        if !literal.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(ParseError {
                position: start,
                kind: ParseErrorKind::InvalidLimit {
                    literal: literal.to_owned(),
                },
            });
        }

        literal.parse().map_err(|_| ParseError {
            position: start,
            kind: ParseErrorKind::LimitOutOfRange {
                literal: literal.to_owned(),
            },
        })
    }

    fn parse_row(&mut self, limits: InsertParseLimits) -> Result<Vec<Value>, ParseError> {
        let mut values = Vec::new();

        loop {
            self.skip_whitespace();
            if self.peek() == Some(b')') {
                return Err(self.error(if values.is_empty() {
                    ParseErrorKind::EmptyRow
                } else {
                    ParseErrorKind::ExpectedValue
                }));
            }
            if values.len() == limits.max_values_per_row {
                return Err(self.error(ParseErrorKind::TooManyValues {
                    limit: limits.max_values_per_row,
                }));
            }

            values.push(self.parse_value(limits.max_string_bytes)?);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.position += 1,
                Some(b')') => {
                    self.position += 1;
                    return Ok(values);
                }
                _ => {
                    return Err(self.error(ParseErrorKind::ExpectedToken {
                        expected: "',' or ')'",
                    }));
                }
            }
        }
    }

    fn parse_value(&mut self, max_string_bytes: usize) -> Result<Value, ParseError> {
        self.skip_whitespace();
        let start = self.position;
        if self.peek() == Some(b'\'') {
            return self.parse_string(max_string_bytes).map(Value::String);
        }

        let literal = self.take_token();
        if literal.is_empty() {
            return Err(ParseError {
                position: start,
                kind: ParseErrorKind::ExpectedValue,
            });
        }
        if literal.eq_ignore_ascii_case("true") {
            return Ok(Value::Bool(true));
        }
        if literal.eq_ignore_ascii_case("false") {
            return Ok(Value::Bool(false));
        }

        match numeric_literal_kind(literal) {
            Some(NumericLiteralKind::Integer) => {
                literal
                    .parse::<i64>()
                    .map(Value::Int64)
                    .map_err(|_| ParseError {
                        position: start,
                        kind: ParseErrorKind::IntegerLiteralOutOfRange {
                            literal: literal.to_owned(),
                        },
                    })
            }
            Some(NumericLiteralKind::Float) => {
                let value = literal.parse::<f64>().map_err(|_| ParseError {
                    position: start,
                    kind: ParseErrorKind::InvalidLiteral {
                        literal: literal.to_owned(),
                    },
                })?;
                if value.is_finite() {
                    Ok(Value::Float64(value))
                } else {
                    Err(ParseError {
                        position: start,
                        kind: ParseErrorKind::FloatLiteralOutOfRange {
                            literal: literal.to_owned(),
                        },
                    })
                }
            }
            None => Err(ParseError {
                position: start,
                kind: ParseErrorKind::InvalidLiteral {
                    literal: literal.to_owned(),
                },
            }),
        }
    }

    fn parse_string(&mut self, max_bytes: usize) -> Result<String, ParseError> {
        self.position += 1;
        let mut value = String::new();

        loop {
            let segment_start = self.position;
            while self.peek().is_some_and(|byte| byte != b'\'') {
                self.position += 1;
            }

            let segment = &self.input[segment_start..self.position];
            let remaining = max_bytes.saturating_sub(value.len());
            if segment.len() > remaining {
                return Err(ParseError {
                    position: segment_start + remaining,
                    kind: ParseErrorKind::StringTooLong { limit: max_bytes },
                });
            }
            value.push_str(segment);

            if self.peek().is_none() {
                return Err(self.error(ParseErrorKind::UnterminatedString));
            }
            if self.input.as_bytes().get(self.position + 1) == Some(&b'\'') {
                if value.len() == max_bytes {
                    return Err(self.error(ParseErrorKind::StringTooLong { limit: max_bytes }));
                }
                value.push('\'');
                self.position += 2;
            } else {
                self.position += 1;
                return Ok(value);
            }
        }
    }

    fn finish_statement(&mut self) -> Result<(), ParseError> {
        self.skip_whitespace();
        if self.peek() == Some(b';') {
            self.position += 1;
            self.skip_whitespace();
        }
        if self.peek().is_some() {
            return Err(self.error(ParseErrorKind::TrailingSyntax));
        }
        Ok(())
    }

    fn parse_keyword(&mut self, expected: &'static str) -> Result<(), ParseError> {
        self.skip_whitespace();
        let start = self.position;
        let found = self.take_token();
        if found.eq_ignore_ascii_case(expected) {
            Ok(())
        } else {
            Err(ParseError {
                position: start,
                kind: ParseErrorKind::ExpectedKeyword {
                    expected,
                    found: (!found.is_empty()).then(|| found.to_owned()),
                },
            })
        }
    }

    fn parse_identifier(
        &mut self,
        context: IdentifierContext,
    ) -> Result<(String, usize), ParseError> {
        self.skip_whitespace();
        let start = self.position;
        let identifier = self.take_token();
        if identifier.is_empty() {
            return Err(ParseError {
                position: start,
                kind: ParseErrorKind::ExpectedIdentifier { context },
            });
        }

        if let Some(offset) = invalid_identifier_offset(identifier) {
            return Err(ParseError {
                position: start + offset,
                kind: ParseErrorKind::InvalidIdentifier {
                    context,
                    identifier: identifier.to_owned(),
                },
            });
        }

        Ok((identifier.to_owned(), start))
    }

    fn parse_data_type(&mut self) -> Result<DataType, ParseError> {
        self.skip_whitespace();
        let start = self.position;
        let type_name = self.take_token();
        if type_name.is_empty() {
            return Err(ParseError {
                position: start,
                kind: ParseErrorKind::ExpectedType,
            });
        }

        if type_name.eq_ignore_ascii_case("Int64") {
            Ok(DataType::Int64)
        } else if type_name.eq_ignore_ascii_case("Float64") {
            Ok(DataType::Float64)
        } else if type_name.eq_ignore_ascii_case("Bool") {
            Ok(DataType::Bool)
        } else if type_name.eq_ignore_ascii_case("String") {
            Ok(DataType::String)
        } else {
            Err(ParseError {
                position: start,
                kind: ParseErrorKind::UnknownType {
                    type_name: type_name.to_owned(),
                },
            })
        }
    }

    fn expect_byte(&mut self, byte: u8, expected: &'static str) -> Result<(), ParseError> {
        self.skip_whitespace();
        if self.peek() == Some(byte) {
            self.position += 1;
            Ok(())
        } else {
            Err(self.error(ParseErrorKind::ExpectedToken { expected }))
        }
    }

    fn take_token(&mut self) -> &'a str {
        let start = self.position;
        while let Some(byte) = self.peek() {
            if is_whitespace(byte) || matches!(byte, b'(' | b')' | b',' | b';' | b'*') {
                break;
            }
            self.position += 1;
        }
        &self.input[start..self.position]
    }

    fn peek_token_is(&self, expected: &str) -> bool {
        let bytes = self.input.as_bytes();
        let mut position = self.position;
        while bytes.get(position).copied().is_some_and(is_whitespace) {
            position += 1;
        }
        let start = position;
        while let Some(byte) = bytes.get(position) {
            if is_whitespace(*byte) || matches!(byte, b'(' | b')' | b',' | b';' | b'*') {
                break;
            }
            position += 1;
        }
        self.input[start..position].eq_ignore_ascii_case(expected)
    }

    fn skip_whitespace(&mut self) {
        while self.peek().is_some_and(is_whitespace) {
            self.position += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.as_bytes().get(self.position).copied()
    }

    fn error(&self, kind: ParseErrorKind) -> ParseError {
        ParseError {
            position: self.position,
            kind,
        }
    }
}

fn invalid_identifier_offset(identifier: &str) -> Option<usize> {
    let bytes = identifier.as_bytes();
    if !bytes
        .first()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
    {
        return Some(0);
    }

    bytes
        .iter()
        .position(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_')
}

#[derive(Clone, Copy)]
enum NumericLiteralKind {
    Integer,
    Float,
}

fn numeric_literal_kind(literal: &str) -> Option<NumericLiteralKind> {
    let bytes = literal.as_bytes();
    let mut position = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let integer_start = position;
    while bytes.get(position).is_some_and(u8::is_ascii_digit) {
        position += 1;
    }
    let integer_digits = position - integer_start;

    let mut kind = NumericLiteralKind::Integer;
    let mut fractional_digits = 0;
    if bytes.get(position) == Some(&b'.') {
        kind = NumericLiteralKind::Float;
        position += 1;
        let fractional_start = position;
        while bytes.get(position).is_some_and(u8::is_ascii_digit) {
            position += 1;
        }
        fractional_digits = position - fractional_start;
    }
    if integer_digits == 0 && fractional_digits == 0 {
        return None;
    }

    if matches!(bytes.get(position), Some(b'e' | b'E')) {
        kind = NumericLiteralKind::Float;
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

fn is_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}
