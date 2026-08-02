use std::collections::HashSet;

use unicode_casefold::UnicodeCaseFold;

use crate::{DataType, DatabaseError, LimitKind, Limits, Value};

/// A named, typed column in a table schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDefinition {
    pub name: String,
    pub data_type: DataType,
}

/// The ordered columns of a table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    columns: Vec<ColumnDefinition>,
    quoted: Vec<bool>,
}

impl Schema {
    pub fn new(columns: Vec<ColumnDefinition>, limits: &Limits) -> Result<Self, DatabaseError> {
        let quoted = vec![false; columns.len()];
        Self::new_with_quoted(columns, quoted, limits)
    }

    pub(crate) fn new_with_quoted(
        columns: Vec<ColumnDefinition>,
        quoted: Vec<bool>,
        limits: &Limits,
    ) -> Result<Self, DatabaseError> {
        if columns.is_empty() {
            return Err(DatabaseError::invalid(
                "a table must have at least one column",
            ));
        }
        if columns.len() > limits.max_columns_per_table {
            return Err(DatabaseError::LimitExceeded {
                kind: LimitKind::ColumnsPerTable,
                limit: limits.max_columns_per_table,
                actual: columns.len(),
            });
        }

        debug_assert_eq!(columns.len(), quoted.len());
        let mut names = HashSet::with_capacity(columns.len());
        for (column, quoted) in columns.iter().zip(&quoted) {
            let normalized = identifier_key(&column.name, *quoted);
            if !names.insert(normalized) {
                return Err(DatabaseError::ColumnAlreadyExists(column.name.clone()));
            }
        }
        Ok(Self { columns, quoted })
    }

    pub fn columns(&self) -> &[ColumnDefinition] {
        &self.columns
    }

    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.column_index_bound(name, false)
    }

    pub(crate) fn column_index_bound(&self, name: &str, quoted: bool) -> Option<usize> {
        let normalized = identifier_key(name, quoted);
        self.columns
            .iter()
            .zip(&self.quoted)
            .position(|(column, column_quoted)| {
                identifier_key(&column.name, *column_quoted) == normalized
            })
    }

    pub(crate) fn column_is_quoted(&self, index: usize) -> bool {
        self.quoted[index]
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Column {
    Int64(Vec<i64>),
    Float64(Vec<f64>),
    Bool(Vec<bool>),
    String(Vec<String>),
}

impl Column {
    fn new(data_type: DataType) -> Self {
        match data_type {
            DataType::Int64 => Self::Int64(Vec::new()),
            DataType::Float64 => Self::Float64(Vec::new()),
            DataType::Bool => Self::Bool(Vec::new()),
            DataType::String => Self::String(Vec::new()),
        }
    }

    fn push(&mut self, value: &Value) {
        match (self, value) {
            (Self::Int64(values), Value::Int64(value)) => values.push(*value),
            (Self::Float64(values), Value::Float64(value)) => values.push(*value),
            (Self::Bool(values), Value::Bool(value)) => values.push(*value),
            (Self::String(values), Value::String(value)) => values.push(value.clone()),
            _ => unreachable!("values are validated before columns are mutated"),
        }
    }

    pub(crate) fn value(&self, row: usize) -> Value {
        match self {
            Self::Int64(values) => Value::Int64(values[row]),
            Self::Float64(values) => Value::Float64(values[row]),
            Self::Bool(values) => Value::Bool(values[row]),
            Self::String(values) => Value::String(values[row].clone()),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Table {
    pub(crate) name: String,
    pub(crate) name_quoted: bool,
    pub(crate) schema: Schema,
    columns: Vec<Column>,
    row_count: usize,
}

impl Table {
    pub(crate) fn new(name: String, name_quoted: bool, schema: Schema) -> Self {
        let columns = schema
            .columns()
            .iter()
            .map(|definition| Column::new(definition.data_type))
            .collect();
        Self {
            name,
            name_quoted,
            schema,
            columns,
            row_count: 0,
        }
    }

    pub(crate) fn row_count(&self) -> usize {
        self.row_count
    }

    pub(crate) fn value(&self, column: usize, row: usize) -> Value {
        self.columns[column].value(row)
    }

    /// Validates the entire batch before appending any value.
    pub(crate) fn append_rows(
        &mut self,
        rows: &[Vec<Value>],
        limits: &Limits,
    ) -> Result<(), DatabaseError> {
        if rows.len() > limits.max_rows_per_insert {
            return Err(DatabaseError::LimitExceeded {
                kind: LimitKind::RowsPerInsert,
                limit: limits.max_rows_per_insert,
                actual: rows.len(),
            });
        }
        let final_rows = self
            .row_count
            .checked_add(rows.len())
            .ok_or_else(|| DatabaseError::ArithmeticOverflow("computing table row count".into()))?;
        if final_rows > limits.max_rows_per_table {
            return Err(DatabaseError::LimitExceeded {
                kind: LimitKind::RowsPerTable,
                limit: limits.max_rows_per_table,
                actual: final_rows,
            });
        }

        for (row_index, row) in rows.iter().enumerate() {
            if row.len() != self.columns.len() {
                return Err(DatabaseError::InvalidValue(format!(
                    "row {} has {} values but table {} expects {}",
                    row_index + 1,
                    row.len(),
                    self.name,
                    self.columns.len()
                )));
            }
            for (column, (value, definition)) in row.iter().zip(self.schema.columns()).enumerate() {
                if value.data_type() != definition.data_type {
                    return Err(DatabaseError::TypeMismatch {
                        context: format!(
                            "row {}, column {} ({})",
                            row_index + 1,
                            column + 1,
                            definition.name
                        ),
                        expected: definition.data_type,
                        actual: value.data_type(),
                    });
                }
                if let Value::String(value) = value
                    && value.len() > limits.max_string_bytes
                {
                    return Err(DatabaseError::LimitExceeded {
                        kind: LimitKind::StringBytes,
                        limit: limits.max_string_bytes,
                        actual: value.len(),
                    });
                }
            }
        }

        for row in rows {
            for (column, value) in self.columns.iter_mut().zip(row) {
                column.push(value);
            }
        }
        self.row_count = final_rows;
        Ok(())
    }
}

pub(crate) fn normalize_identifier(name: &str) -> String {
    name.case_fold().collect()
}

pub(crate) fn identifier_key(name: &str, quoted: bool) -> String {
    if quoted {
        name.to_owned()
    } else {
        normalize_identifier(name)
    }
}

pub(crate) fn identifiers_equal(
    left: &str,
    left_quoted: bool,
    right: &str,
    right_quoted: bool,
) -> bool {
    identifier_key(left, left_quoted) == identifier_key(right, right_quoted)
}
