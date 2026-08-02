//! Execution of bounded `INSERT INTO <table> VALUES` statements.

use std::error::Error;
use std::fmt;

use crate::storage::{DataType, Row, StorageError, Table, Value};

use super::lexer::{
    LexError, LexerConfig, Operator, Position, Punctuation, SpannedToken, TokenKind,
    tokenize_with_config,
};

/// Default maximum number of rows accepted by one insert statement.
pub const DEFAULT_MAX_INSERT_ROWS: usize = 100_000;

/// Default maximum number of scalar values accepted by one insert statement.
pub const DEFAULT_MAX_INSERT_VALUES: usize = 1_000_000;

/// Resource limits applied while decoding an insert statement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InsertLimits {
    /// Byte and token limits applied before parsing.
    pub lexer: LexerConfig,
    /// Maximum number of parenthesized rows.
    pub max_rows: usize,
    /// Maximum total number of values across all rows.
    pub max_values: usize,
}

impl Default for InsertLimits {
    fn default() -> Self {
        Self {
            lexer: LexerConfig::default(),
            max_rows: DEFAULT_MAX_INSERT_ROWS,
            max_values: DEFAULT_MAX_INSERT_VALUES,
        }
    }
}

/// A failure to parse, validate, or store a SQL values insert.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InsertError {
    Lex(LexError),
    Syntax {
        expected: &'static str,
        found: Option<TokenKind>,
        position: Position,
    },
    TableNameMismatch {
        expected: String,
        actual: String,
        position: Position,
    },
    RowLimitExceeded {
        limit: usize,
        position: Position,
    },
    ValueLimitExceeded {
        limit: usize,
        position: Position,
    },
    RowWidth {
        row: usize,
        expected: usize,
        actual: usize,
        position: Position,
    },
    InvalidValue {
        row: usize,
        column: usize,
        expected: DataType,
        found: TokenKind,
        position: Position,
    },
    InvalidNumber {
        row: usize,
        column: usize,
        expected: DataType,
        literal: String,
        position: Position,
    },
    Storage(StorageError),
}

impl InsertError {
    /// Returns the source position for SQL-originated errors.
    pub fn position(&self) -> Option<Position> {
        match self {
            Self::Lex(error) => Some(error.position()),
            Self::Syntax { position, .. }
            | Self::TableNameMismatch { position, .. }
            | Self::RowLimitExceeded { position, .. }
            | Self::ValueLimitExceeded { position, .. }
            | Self::RowWidth { position, .. }
            | Self::InvalidValue { position, .. }
            | Self::InvalidNumber { position, .. } => Some(*position),
            Self::Storage(_) => None,
        }
    }
}

impl fmt::Display for InsertError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lex(error) => error.fmt(formatter),
            Self::Syntax {
                expected,
                found,
                position,
            } => match found {
                Some(found) => write!(
                    formatter,
                    "expected {expected} at {position}, found {}",
                    TokenDescription(found)
                ),
                None => write!(
                    formatter,
                    "expected {expected} at {position}, found end of input"
                ),
            },
            Self::TableNameMismatch {
                expected,
                actual,
                position,
            } => write!(
                formatter,
                "insert targets table {actual:?} at {position}, but the provided table is {expected:?}"
            ),
            Self::RowLimitExceeded { limit, position } => {
                write!(
                    formatter,
                    "insert row limit of {limit} exceeded at {position}"
                )
            }
            Self::ValueLimitExceeded { limit, position } => write!(
                formatter,
                "insert value limit of {limit} exceeded at {position}"
            ),
            Self::RowWidth {
                row,
                expected,
                actual,
                position,
            } => write!(
                formatter,
                "insert row {row} has {actual} values at {position}, but the schema requires {expected}"
            ),
            Self::InvalidValue {
                row,
                column,
                expected,
                found,
                position,
            } => write!(
                formatter,
                "insert row {row}, column {column} requires {expected} at {position}, found {}",
                TokenDescription(found)
            ),
            Self::InvalidNumber {
                row,
                column,
                expected,
                literal,
                position,
            } => write!(
                formatter,
                "insert row {row}, column {column} has invalid {expected} literal {literal:?} at {position}"
            ),
            Self::Storage(error) => write!(formatter, "insert failed: {error}"),
        }
    }
}

impl Error for InsertError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Lex(error) => Some(error),
            Self::Storage(error) => Some(error),
            _ => None,
        }
    }
}

impl From<LexError> for InsertError {
    fn from(error: LexError) -> Self {
        Self::Lex(error)
    }
}

impl From<StorageError> for InsertError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

/// Executes one values insert using the default resource limits.
///
/// `table_name` names the supplied table because RustHouse does not yet have a
/// catalog. Unquoted SQL identifiers are compared ASCII-case-insensitively.
/// The return value is the number of inserted rows.
pub fn execute_insert(
    input: &str,
    table_name: &str,
    table: &mut Table,
) -> Result<usize, InsertError> {
    execute_insert_with_limits(input, table_name, table, InsertLimits::default())
}

/// Executes one values insert using explicit lexer, row, and value limits.
///
/// Parsing and decoding finish before the table is touched. The decoded rows
/// are passed to [`Table::insert_batch`] in one call, preserving batch
/// atomicity when storage validation fails.
pub fn execute_insert_with_limits(
    input: &str,
    table_name: &str,
    table: &mut Table,
    limits: InsertLimits,
) -> Result<usize, InsertError> {
    let tokens = tokenize_with_config(input, limits.lexer)?;
    let rows = Parser::new(&tokens, table_name, table, limits).parse()?;
    let inserted_rows = rows.len();
    table.insert_batch(rows)?;
    Ok(inserted_rows)
}

struct Parser<'a> {
    tokens: &'a [SpannedToken],
    cursor: usize,
    table_name: &'a str,
    table: &'a Table,
    limits: InsertLimits,
    value_count: usize,
}

impl<'a> Parser<'a> {
    fn new(
        tokens: &'a [SpannedToken],
        table_name: &'a str,
        table: &'a Table,
        limits: InsertLimits,
    ) -> Self {
        Self {
            tokens,
            cursor: 0,
            table_name,
            table,
            limits,
            value_count: 0,
        }
    }

    fn parse(mut self) -> Result<Vec<Row>, InsertError> {
        self.expect_keyword("INSERT")?;
        self.expect_keyword("INTO")?;
        self.expect_table_name()?;
        self.expect_keyword("VALUES")?;

        let mut rows = Vec::with_capacity(self.limits.max_rows.min(1024));
        loop {
            let row_position = self.current_position();
            if rows.len() == self.limits.max_rows {
                return Err(InsertError::RowLimitExceeded {
                    limit: self.limits.max_rows,
                    position: row_position,
                });
            }
            rows.push(self.parse_row(rows.len())?);

            if !self.consume_punctuation(Punctuation::Comma) {
                break;
            }
        }

        self.consume_semicolon();
        if self.current().is_some() {
            return Err(self.syntax("end of input"));
        }
        Ok(rows)
    }

    fn expect_table_name(&mut self) -> Result<(), InsertError> {
        let token = self
            .current()
            .ok_or_else(|| self.syntax("a table identifier"))?;
        let TokenKind::Identifier(actual) = &token.kind else {
            return Err(self.syntax("a table identifier"));
        };
        if !actual.eq_ignore_ascii_case(self.table_name) {
            return Err(InsertError::TableNameMismatch {
                expected: self.table_name.to_owned(),
                actual: actual.clone(),
                position: token.span.start,
            });
        }
        self.cursor += 1;
        Ok(())
    }

    fn parse_row(&mut self, row_index: usize) -> Result<Row, InsertError> {
        let row_position = self.current_position();
        self.expect_punctuation(Punctuation::LeftParenthesis, "'('")?;
        let mut row = Vec::with_capacity(self.table.schema().len().min(1024));
        let mut parsed_values = 0;

        if !self.at_punctuation(Punctuation::RightParenthesis) {
            loop {
                if self.value_count == self.limits.max_values {
                    return Err(InsertError::ValueLimitExceeded {
                        limit: self.limits.max_values,
                        position: self.current_position(),
                    });
                }

                let column_index = parsed_values;
                let Some(column) = self.table.schema().columns().get(column_index) else {
                    self.consume_untyped_value()?;
                    self.value_count += 1;
                    parsed_values += 1;
                    if self.consume_punctuation(Punctuation::Comma) {
                        continue;
                    }
                    break;
                };

                row.push(self.parse_value(row_index, column_index, column.data_type())?);
                self.value_count += 1;
                parsed_values += 1;
                if !self.consume_punctuation(Punctuation::Comma) {
                    break;
                }
            }
        }

        self.expect_punctuation(Punctuation::RightParenthesis, "')'")?;
        if parsed_values != self.table.schema().len() {
            return Err(InsertError::RowWidth {
                row: row_index,
                expected: self.table.schema().len(),
                actual: parsed_values,
                position: row_position,
            });
        }
        Ok(row)
    }

    fn parse_value(
        &mut self,
        row: usize,
        column: usize,
        expected: DataType,
    ) -> Result<Value, InsertError> {
        let position = self.current_position();
        match expected {
            DataType::Int64 => {
                let literal = self.parse_number_literal(row, column, expected)?;
                literal
                    .parse::<i64>()
                    .map(Value::Int64)
                    .map_err(|_| InsertError::InvalidNumber {
                        row,
                        column,
                        expected,
                        literal,
                        position,
                    })
            }
            DataType::Float64 => {
                let literal = self.parse_number_literal(row, column, expected)?;
                literal
                    .parse::<f64>()
                    .map(Value::Float64)
                    .map_err(|_| InsertError::InvalidNumber {
                        row,
                        column,
                        expected,
                        literal,
                        position,
                    })
            }
            DataType::Bool => {
                let token = self.take_value_token(row, column, expected)?;
                match &token.kind {
                    TokenKind::Identifier(value) if value.eq_ignore_ascii_case("true") => {
                        Ok(Value::Bool(true))
                    }
                    TokenKind::Identifier(value) if value.eq_ignore_ascii_case("false") => {
                        Ok(Value::Bool(false))
                    }
                    _ => Err(InsertError::InvalidValue {
                        row,
                        column,
                        expected,
                        found: token.kind.clone(),
                        position: token.span.start,
                    }),
                }
            }
            DataType::String => {
                let token = self.take_value_token(row, column, expected)?;
                match &token.kind {
                    TokenKind::String(value) => Ok(Value::String(value.clone())),
                    _ => Err(InsertError::InvalidValue {
                        row,
                        column,
                        expected,
                        found: token.kind.clone(),
                        position: token.span.start,
                    }),
                }
            }
        }
    }

    fn parse_number_literal(
        &mut self,
        row: usize,
        column: usize,
        expected: DataType,
    ) -> Result<String, InsertError> {
        let sign = match self.current().map(|token| &token.kind) {
            Some(TokenKind::Operator(Operator::Minus)) => {
                self.cursor += 1;
                "-"
            }
            Some(TokenKind::Operator(Operator::Plus)) => {
                self.cursor += 1;
                "+"
            }
            _ => "",
        };

        let token = self.take_value_token(row, column, expected)?;
        match &token.kind {
            TokenKind::Number(number) => Ok(format!("{sign}{number}")),
            _ => Err(InsertError::InvalidValue {
                row,
                column,
                expected,
                found: token.kind.clone(),
                position: token.span.start,
            }),
        }
    }

    fn consume_untyped_value(&mut self) -> Result<(), InsertError> {
        if matches!(
            self.current().map(|token| &token.kind),
            Some(TokenKind::Operator(Operator::Plus | Operator::Minus))
        ) {
            self.cursor += 1;
            if !matches!(
                self.current().map(|token| &token.kind),
                Some(TokenKind::Number(_))
            ) {
                return Err(self.syntax("a number after a sign"));
            }
            self.cursor += 1;
            return Ok(());
        }

        match self.current().map(|token| &token.kind) {
            Some(TokenKind::Number(_) | TokenKind::String(_)) => {
                self.cursor += 1;
                Ok(())
            }
            Some(TokenKind::Identifier(value))
                if value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("false") =>
            {
                self.cursor += 1;
                Ok(())
            }
            _ => Err(self.syntax("a literal value")),
        }
    }

    fn take_value_token(
        &mut self,
        row: usize,
        column: usize,
        expected: DataType,
    ) -> Result<&'a SpannedToken, InsertError> {
        let token = self
            .current()
            .ok_or_else(|| self.syntax("a literal value"))?;
        if matches!(
            token.kind,
            TokenKind::Punctuation(Punctuation::Comma | Punctuation::RightParenthesis)
        ) {
            return Err(InsertError::InvalidValue {
                row,
                column,
                expected,
                found: token.kind.clone(),
                position: token.span.start,
            });
        }
        self.cursor += 1;
        Ok(token)
    }

    fn expect_keyword(&mut self, expected: &'static str) -> Result<(), InsertError> {
        let Some(token) = self.current() else {
            return Err(self.syntax(expected));
        };
        match &token.kind {
            TokenKind::Identifier(identifier) if identifier.eq_ignore_ascii_case(expected) => {
                self.cursor += 1;
                Ok(())
            }
            _ => Err(self.syntax(expected)),
        }
    }

    fn expect_punctuation(
        &mut self,
        punctuation: Punctuation,
        expected: &'static str,
    ) -> Result<(), InsertError> {
        if self.consume_punctuation(punctuation) {
            Ok(())
        } else {
            Err(self.syntax(expected))
        }
    }

    fn at_punctuation(&self, punctuation: Punctuation) -> bool {
        matches!(
            self.current().map(|token| &token.kind),
            Some(TokenKind::Punctuation(actual)) if *actual == punctuation
        )
    }

    fn consume_punctuation(&mut self, punctuation: Punctuation) -> bool {
        if self.at_punctuation(punctuation) {
            self.cursor += 1;
            true
        } else {
            false
        }
    }

    fn consume_semicolon(&mut self) -> bool {
        if matches!(
            self.current().map(|token| &token.kind),
            Some(TokenKind::Semicolon)
        ) {
            self.cursor += 1;
            true
        } else {
            false
        }
    }

    fn current(&self) -> Option<&'a SpannedToken> {
        self.tokens.get(self.cursor)
    }

    fn current_position(&self) -> Position {
        self.current()
            .map_or_else(|| self.end_position(), |token| token.span.start)
    }

    fn end_position(&self) -> Position {
        self.tokens.last().map_or(
            Position {
                byte_offset: 0,
                line: 1,
                column: 1,
            },
            |token| token.span.end,
        )
    }

    fn syntax(&self, expected: &'static str) -> InsertError {
        InsertError::Syntax {
            expected,
            found: self.current().map(|token| token.kind.clone()),
            position: self.current_position(),
        }
    }
}

struct TokenDescription<'a>(&'a TokenKind);

impl fmt::Display for TokenDescription<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            TokenKind::Identifier(value) => write!(formatter, "identifier {value:?}"),
            TokenKind::Number(value) => write!(formatter, "number {value:?}"),
            TokenKind::String(_) => formatter.write_str("a string"),
            TokenKind::Operator(operator) => write!(formatter, "operator {operator:?}"),
            TokenKind::Punctuation(punctuation) => {
                write!(formatter, "punctuation {punctuation:?}")
            }
            TokenKind::Semicolon => formatter.write_str("';'"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{Column, ColumnSchema, Schema};

    fn events_table(capacity: usize) -> Table {
        Table::new(
            Schema::new(vec![
                ColumnSchema::new("ts", DataType::Int64),
                ColumnSchema::new("user_id", DataType::Int64),
                ColumnSchema::new("path", DataType::String),
                ColumnSchema::new("value", DataType::Float64),
                ColumnSchema::new("active", DataType::Bool),
            ]),
            capacity,
        )
    }

    #[test]
    fn executes_benchmark_shaped_values_as_one_batch() {
        let mut table = events_table(3);
        let inserted = execute_insert(
            "INSERT INTO events VALUES\n\
             (1, 10, '/docs', 12.5, true),\n\
             (-2, 11, 'O''Reilly', -3.25e1, FALSE);",
            "events",
            &mut table,
        )
        .expect("valid values insert should execute");

        assert_eq!(inserted, 2);
        assert_eq!(
            table.columns(),
            &[
                Column::Int64(vec![1, -2]),
                Column::Int64(vec![10, 11]),
                Column::String(vec!["/docs".into(), "O'Reilly".into()]),
                Column::Float64(vec![12.5, -32.5]),
                Column::Bool(vec![true, false]),
            ]
        );
    }

    #[test]
    fn rejects_malformed_and_multiple_statements_without_mutation() {
        let cases = [
            "INSERT events VALUES (1, 2, 'x', 3.0, true)",
            "INSERT INTO events VALUES (1, 2, 'x', 3.0, true),",
            "INSERT INTO events VALUES (1, 2, 'x', 3.0, true); INSERT INTO events VALUES (2, 3, 'y', 4.0, false)",
            "INSERT INTO events VALUES (1, 2, 'x', 3.0, true);;",
        ];

        for sql in cases {
            let mut table = events_table(2);
            assert!(matches!(
                execute_insert(sql, "events", &mut table),
                Err(InsertError::Syntax { .. })
            ));
            assert!(table.is_empty(), "malformed input mutated table: {sql}");
        }
    }

    #[test]
    fn rejects_table_width_and_literal_type_mismatches() {
        let mut table = events_table(2);
        assert!(matches!(
            execute_insert(
                "INSERT INTO other VALUES (1, 2, 'x', 3.0, true)",
                "events",
                &mut table
            ),
            Err(InsertError::TableNameMismatch { .. })
        ));
        assert!(matches!(
            execute_insert(
                "INSERT INTO events VALUES (1, 2, 'x', true)",
                "events",
                &mut table
            ),
            Err(InsertError::InvalidValue {
                row: 0,
                column: 3,
                expected: DataType::Float64,
                ..
            })
        ));
        assert!(matches!(
            execute_insert(
                "INSERT INTO events VALUES (1, 2, 'x', 3.0)",
                "events",
                &mut table
            ),
            Err(InsertError::RowWidth {
                row: 0,
                expected: 5,
                actual: 4,
                ..
            })
        ));
        assert!(table.is_empty());
    }

    #[test]
    fn enforces_row_value_and_lexer_limits() {
        let sql = "INSERT INTO events VALUES (1, 2, 'x', 3.0, true), (2, 3, 'y', 4.0, false)";
        let mut table = events_table(3);

        let row_error = execute_insert_with_limits(
            sql,
            "events",
            &mut table,
            InsertLimits {
                max_rows: 1,
                ..InsertLimits::default()
            },
        )
        .unwrap_err();
        assert!(matches!(
            row_error,
            InsertError::RowLimitExceeded { limit: 1, .. }
        ));

        let value_error = execute_insert_with_limits(
            sql,
            "events",
            &mut table,
            InsertLimits {
                max_values: 4,
                ..InsertLimits::default()
            },
        )
        .unwrap_err();
        assert!(matches!(
            value_error,
            InsertError::ValueLimitExceeded { limit: 4, .. }
        ));

        let lex_error = execute_insert_with_limits(
            sql,
            "events",
            &mut table,
            InsertLimits {
                lexer: LexerConfig {
                    max_input_bytes: sql.len() - 1,
                    max_tokens: usize::MAX,
                },
                ..InsertLimits::default()
            },
        )
        .unwrap_err();
        assert!(matches!(
            lex_error,
            InsertError::Lex(LexError::InputTooLarge { .. })
        ));
        assert!(table.is_empty());
    }

    #[test]
    fn accepts_exact_row_and_value_limits() {
        let sql = "INSERT INTO events VALUES (1, 2, 'x', 3, true), (2, 3, 'y', 4, false)";
        let mut table = events_table(2);

        let inserted = execute_insert_with_limits(
            sql,
            "EVENTS",
            &mut table,
            InsertLimits {
                max_rows: 2,
                max_values: 10,
                ..InsertLimits::default()
            },
        )
        .unwrap();

        assert_eq!(inserted, 2);
        assert_eq!(table.len(), 2);
        assert_eq!(table.columns()[3], Column::Float64(vec![3.0, 4.0]));
    }

    #[test]
    fn handles_numeric_boundaries_and_rolls_back_non_finite_batches() {
        let schema = Schema::new(vec![
            ColumnSchema::new("integer", DataType::Int64),
            ColumnSchema::new("float", DataType::Float64),
        ]);
        let mut table = Table::new(schema, 3);

        execute_insert(
            "INSERT INTO numbers VALUES (-9223372036854775808, +1.5)",
            "numbers",
            &mut table,
        )
        .unwrap();
        let before = table.clone();

        let overflow = execute_insert(
            "INSERT INTO numbers VALUES (9223372036854775808, 2.0)",
            "numbers",
            &mut table,
        )
        .unwrap_err();
        assert!(matches!(
            overflow,
            InsertError::InvalidNumber {
                row: 0,
                column: 0,
                expected: DataType::Int64,
                ..
            }
        ));
        assert_eq!(table, before);

        let non_finite = execute_insert(
            "INSERT INTO numbers VALUES (2, 2.0), (3, 1e309)",
            "numbers",
            &mut table,
        )
        .unwrap_err();
        assert_eq!(
            non_finite,
            InsertError::Storage(StorageError::NonFiniteFloat { row: 1, column: 1 })
        );
        assert_eq!(table, before);
    }

    #[test]
    fn storage_failure_rolls_back_the_entire_sql_batch() {
        let mut table = events_table(2);
        execute_insert(
            "INSERT INTO events VALUES (1, 2, 'existing', 3.0, true)",
            "events",
            &mut table,
        )
        .unwrap();
        let before = table.clone();

        let error = execute_insert(
            "INSERT INTO events VALUES (2, 3, 'valid', 4.0, false), (3, 4, 'also valid', 5.0, true)",
            "events",
            &mut table,
        )
        .unwrap_err();

        assert_eq!(
            error,
            InsertError::Storage(StorageError::CapacityExceeded {
                capacity: 2,
                current_rows: 1,
                batch_rows: 2,
            })
        );
        assert_eq!(table, before);
    }

    #[test]
    fn parse_failure_in_a_later_row_rolls_back_earlier_rows() {
        let mut table = events_table(3);
        let before = table.clone();

        let error = execute_insert(
            "INSERT INTO events VALUES (1, 2, 'valid', 3.0, true), (2, 3, 'invalid', 4.0, 'not bool')",
            "events",
            &mut table,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            InsertError::InvalidValue {
                row: 1,
                column: 4,
                expected: DataType::Bool,
                ..
            }
        ));
        assert_eq!(table, before);
    }
}
