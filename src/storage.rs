use std::collections::HashSet;

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
}

impl Schema {
    pub fn new(columns: Vec<ColumnDefinition>, limits: &Limits) -> Result<Self, DatabaseError> {
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

        let mut names = HashSet::with_capacity(columns.len());
        for column in &columns {
            let normalized = normalize_identifier(&column.name);
            if !names.insert(normalized) {
                return Err(DatabaseError::ColumnAlreadyExists(column.name.clone()));
            }
        }
        Ok(Self { columns })
    }

    pub fn columns(&self) -> &[ColumnDefinition] {
        &self.columns
    }

    pub fn column_index(&self, name: &str) -> Option<usize> {
        let normalized = normalize_identifier(name);
        self.columns
            .iter()
            .position(|column| normalize_identifier(&column.name) == normalized)
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
    pub(crate) schema: Schema,
    columns: Vec<Column>,
    row_count: usize,
}

impl Table {
    pub(crate) fn new(name: String, schema: Schema) -> Self {
        let columns = schema
            .columns()
            .iter()
            .map(|definition| Column::new(definition.data_type))
            .collect();
        Self {
            name,
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
    name.to_ascii_lowercase()
}
