//! Bounded, column-major table storage.

use std::error::Error;
use std::fmt;

use crate::column::Column;
use crate::scalar::{DataType, ScalarValue};
use crate::schema::Schema;

/// An in-memory table with a fixed schema and configured row limit.
#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    schema: Schema,
    columns: Vec<Column>,
    row_limit: usize,
    row_count: usize,
}

impl Table {
    /// Creates an empty table that can hold at most `row_limit` rows.
    #[must_use]
    pub fn new(schema: Schema, row_limit: usize) -> Self {
        let columns = schema
            .fields()
            .iter()
            .map(|field| Column::new(field.data_type()))
            .collect();

        Self {
            schema,
            columns,
            row_limit,
            row_count: 0,
        }
    }

    /// Inserts one row after validating the whole row.
    ///
    /// Arity, every scalar type, and the row limit are checked before any
    /// column is mutated. Therefore every returned error leaves all stored
    /// values unchanged.
    pub fn insert<I>(&mut self, row: I) -> Result<(), InsertError>
    where
        I: IntoIterator<Item = ScalarValue>,
    {
        let mut values = row.into_iter();
        let exact_length = match values.size_hint() {
            (lower, Some(upper)) if lower == upper => Some(lower),
            _ => None,
        };
        let mut row = Vec::with_capacity(self.columns.len());
        row.extend(values.by_ref().take(self.columns.len()));

        if row.len() != self.columns.len() {
            return Err(InsertError::ArityMismatch {
                expected: self.columns.len(),
                actual: row.len(),
            });
        }

        if values.next().is_some() {
            return Err(InsertError::ArityMismatch {
                expected: self.columns.len(),
                actual: exact_length.unwrap_or_else(|| self.columns.len().saturating_add(1)),
            });
        }

        for (index, (field, value)) in self.schema.fields().iter().zip(&row).enumerate() {
            let expected = field.data_type();
            let actual = value.data_type();
            if actual != expected {
                return Err(InsertError::TypeMismatch {
                    column_index: index,
                    column_name: field.name().to_owned(),
                    expected,
                    actual,
                });
            }
        }

        if self.row_count >= self.row_limit {
            return Err(InsertError::RowLimitExceeded {
                limit: self.row_limit,
            });
        }

        for (column, value) in self.columns.iter_mut().zip(row) {
            // The complete row was type-checked above, so this cannot fail.
            column
                .push(value)
                .expect("validated scalar type must match its column");
        }
        self.row_count += 1;
        Ok(())
    }

    /// Returns the table schema.
    #[must_use]
    pub const fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Returns the typed columns in schema order.
    #[must_use]
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// Returns a column by storage index.
    #[must_use]
    pub fn column(&self, index: usize) -> Option<&Column> {
        self.columns.get(index)
    }

    /// Returns a column by case-insensitive field name.
    #[must_use]
    pub fn column_by_name(&self, name: &str) -> Option<&Column> {
        self.schema
            .index_of(name)
            .and_then(|index| self.columns.get(index))
    }

    /// Returns the number of stored rows.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.row_count
    }

    /// Returns whether the table contains no rows.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.row_count == 0
    }

    /// Returns the configured maximum number of rows.
    #[must_use]
    pub const fn row_limit(&self) -> usize {
        self.row_limit
    }
}

/// A row insertion failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InsertError {
    /// The number of values did not match the schema.
    ///
    /// For an overlong iterator without an exact size hint, `actual` is the
    /// observed lower bound of the schema width plus one. The remaining values
    /// are deliberately not consumed.
    ArityMismatch { expected: usize, actual: usize },
    /// A value did not match its field's scalar type.
    TypeMismatch {
        column_index: usize,
        column_name: String,
        expected: DataType,
        actual: DataType,
    },
    /// The table already contains its configured maximum number of rows.
    RowLimitExceeded { limit: usize },
}

impl fmt::Display for InsertError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ArityMismatch { expected, actual } if actual > expected => write!(
                formatter,
                "expected {expected} values, received at least {actual}"
            ),
            Self::ArityMismatch { expected, actual } => {
                write!(formatter, "expected {expected} values, received {actual}")
            }
            Self::TypeMismatch {
                column_index,
                column_name,
                expected,
                actual,
            } => write!(
                formatter,
                "column {column_index} (`{column_name}`) expects {expected}, received {actual}"
            ),
            Self::RowLimitExceeded { limit } => {
                write!(formatter, "table row limit of {limit} has been reached")
            }
        }
    }
}

impl Error for InsertError {}
