//! Parsing and syntax tree types for RustHouse's initial SQL boundary.

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

/// The output requested by a `SELECT` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectProjection {
    /// Expand every column in the table at planning time.
    All,
    /// Return the named columns in the specified order.
    Columns(Vec<String>),
    /// Count every input row, optionally naming the result field.
    CountAll { alias: Option<String> },
    /// Compute one or more scalar aggregates over the same input rows.
    Aggregates(Vec<AggregateProjection>),
    /// Count rows for each distinct value of one projected and grouped key.
    GroupedCount { key: String, alias: Option<String> },
}

/// One supported scalar aggregate expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AggregateFunction {
    /// Count every selected row.
    CountAll,
    /// Count distinct values of one column.
    CountDistinct { column: String },
    /// Sum one numeric column.
    Sum { column: String },
    /// Average one numeric column.
    Avg { column: String },
    /// Find the minimum value of one column.
    Min { column: String },
    /// Find the maximum value of one column.
    Max { column: String },
}

/// One scalar aggregate and its optional output name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggregateProjection {
    pub function: AggregateFunction,
    pub alias: Option<String>,
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

/// The direction applied to one `ORDER BY` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderDirection {
    Ascending,
    Descending,
}

/// One column and direction from an `ORDER BY` clause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderByClause {
    pub column: String,
    pub direction: OrderDirection,
}

/// The syntax tree produced for a bounded `SELECT` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectStatement {
    pub projections: SelectProjection,
    pub table: String,
    /// Disjunctive groups of comparisons. Comparisons within each group are
    /// joined by `AND`; groups are joined by `OR`.
    pub predicate_groups: Vec<Vec<ComparisonPredicate>>,
    pub order_by: Option<Vec<OrderByClause>>,
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
    pub max_predicates: usize,
    pub max_predicate_groups: usize,
    pub max_order_keys: usize,
}

impl SelectParseLimits {
    pub const DEFAULT_MAX_INPUT_BYTES: usize = 1024 * 1024;
    pub const DEFAULT_MAX_PROJECTIONS: usize = 1024;
    pub const DEFAULT_MAX_PREDICATES: usize = 1024;
    pub const DEFAULT_MAX_PREDICATE_GROUPS: usize = 1024;
    pub const DEFAULT_MAX_ORDER_KEYS: usize = 1024;

    pub const fn new(max_input_bytes: usize, max_projections: usize) -> Self {
        Self {
            max_input_bytes,
            max_projections,
            max_predicates: Self::DEFAULT_MAX_PREDICATES,
            max_predicate_groups: Self::DEFAULT_MAX_PREDICATE_GROUPS,
            max_order_keys: Self::DEFAULT_MAX_ORDER_KEYS,
        }
    }

    /// Replaces the maximum total comparisons in a `WHERE` clause.
    #[must_use]
    pub const fn with_max_predicates(mut self, max_predicates: usize) -> Self {
        self.max_predicates = max_predicates;
        self
    }

    /// Replaces the maximum number of `OR` groups in a `WHERE` clause.
    #[must_use]
    pub const fn with_max_predicate_groups(mut self, max_predicate_groups: usize) -> Self {
        self.max_predicate_groups = max_predicate_groups;
        self
    }

    /// Replaces the maximum number of columns in an `ORDER BY` clause.
    #[must_use]
    pub const fn with_max_order_keys(mut self, max_order_keys: usize) -> Self {
        self.max_order_keys = max_order_keys;
        self
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
    MixedAggregateProjection,
    GroupKeyMismatch {
        projected: String,
        grouped: String,
    },
    TooManyProjections {
        limit: usize,
    },
    TooManyPredicates {
        limit: usize,
    },
    TooManyPredicateGroups {
        limit: usize,
    },
    TooManyOrderKeys {
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
            Self::ExpectedProjection => {
                formatter.write_str("expected a column projection, '*', or scalar aggregate")
            }
            Self::MixedAggregateProjection => {
                formatter.write_str("scalar aggregates cannot be mixed with raw column projections")
            }
            Self::GroupKeyMismatch { projected, grouped } => write!(
                formatter,
                "projected group key {projected:?} does not match GROUP BY key {grouped:?}"
            ),
            Self::TooManyProjections { limit } => {
                write!(formatter, "projection count exceeds limit of {limit}")
            }
            Self::TooManyPredicates { limit } => {
                write!(formatter, "predicate count exceeds limit of {limit}")
            }
            Self::TooManyPredicateGroups { limit } => {
                write!(formatter, "predicate group count exceeds limit of {limit}")
            }
            Self::TooManyOrderKeys { limit } => {
                write!(formatter, "order key count exceeds limit of {limit}")
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

mod parser;

pub use parser::{
    parse_create_table, parse_create_table_with_limits, parse_insert, parse_insert_with_limits,
    parse_select, parse_select_with_limits,
};
