use std::collections::HashSet;

use crate::error::{Error, Result};
use crate::value::{DataType, Value, ValueRef};

/// A named, typed field in a table schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: DataType,
}

pub(crate) fn is_reserved_column_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("TRUE") || name.eq_ignore_ascii_case("FALSE")
}

/// A physical column. Each variant owns a contiguous vector of one Rust type.
#[derive(Debug, Clone, PartialEq)]
pub enum Column {
    Int64(Vec<i64>),
    Float64(Vec<f64>),
    Bool(Vec<bool>),
    String(Vec<String>),
}

impl Column {
    #[must_use]
    pub fn new(data_type: DataType) -> Self {
        match data_type {
            DataType::Int64 => Self::Int64(Vec::new()),
            DataType::Float64 => Self::Float64(Vec::new()),
            DataType::Bool => Self::Bool(Vec::new()),
            DataType::String => Self::String(Vec::new()),
        }
    }

    #[must_use]
    pub fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Int64(values) => values.len(),
            Self::Float64(values) => values.len(),
            Self::Bool(values) => values.len(),
            Self::String(values) => values.len(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn with_capacity(data_type: DataType, capacity: usize) -> Self {
        match data_type {
            DataType::Int64 => Self::Int64(Vec::with_capacity(capacity)),
            DataType::Float64 => Self::Float64(Vec::with_capacity(capacity)),
            DataType::Bool => Self::Bool(Vec::with_capacity(capacity)),
            DataType::String => Self::String(Vec::with_capacity(capacity)),
        }
    }

    #[must_use]
    pub fn value(&self, row: usize) -> Value {
        self.value_ref(row).to_owned()
    }

    pub(crate) fn value_ref(&self, row: usize) -> ValueRef<'_> {
        match self {
            Self::Int64(values) => ValueRef::Int64(values[row]),
            Self::Float64(values) => ValueRef::Float64(values[row]),
            Self::Bool(values) => ValueRef::Bool(values[row]),
            Self::String(values) => ValueRef::String(&values[row]),
        }
    }

    pub(crate) fn cmp_at(&self, left: usize, right: usize) -> std::cmp::Ordering {
        self.value_ref(left).cmp(&self.value_ref(right))
    }

    fn push(&mut self, value: Value) {
        match (self, value) {
            (Self::Int64(values), Value::Int64(value)) => values.push(value),
            (Self::Float64(values), Value::Float64(value)) => values.push(value),
            (Self::Bool(values), Value::Bool(value)) => values.push(value),
            (Self::String(values), Value::String(value)) => values.push(value),
            _ => unreachable!("values are validated before insertion"),
        }
    }

    fn contains_non_finite_float(&self) -> bool {
        matches!(self, Self::Float64(values) if values.iter().any(|value| !value.is_finite()))
    }

    fn reserve(&mut self, additional: usize) {
        match self {
            Self::Int64(values) => values.reserve(additional),
            Self::Float64(values) => values.reserve(additional),
            Self::Bool(values) => values.reserve(additional),
            Self::String(values) => values.reserve(additional),
        }
    }

    fn append(&mut self, other: Self) {
        match (self, other) {
            (Self::Int64(values), Self::Int64(mut other)) => values.append(&mut other),
            (Self::Float64(values), Self::Float64(mut other)) => values.append(&mut other),
            (Self::Bool(values), Self::Bool(mut other)) => values.append(&mut other),
            (Self::String(values), Self::String(mut other)) => values.append(&mut other),
            _ => unreachable!("column batches are validated before insertion"),
        }
    }
}

/// An owned collection of typed column buffers ready for bulk insertion.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ColumnBatch {
    columns: Vec<Column>,
}

impl ColumnBatch {
    #[must_use]
    pub fn new(columns: Vec<Column>) -> Self {
        Self { columns }
    }

    #[must_use]
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    #[must_use]
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.columns.iter().all(Column::is_empty)
    }
}

impl From<Vec<Column>> for ColumnBatch {
    fn from(columns: Vec<Column>) -> Self {
        Self::new(columns)
    }
}

/// A table stores one typed vector per schema field.
#[derive(Debug, Clone)]
pub struct Table {
    name: String,
    schema: Vec<ColumnDef>,
    columns: Vec<Column>,
    row_count: usize,
}

impl Table {
    pub fn new(name: String, schema: Vec<ColumnDef>) -> Result<Self> {
        if schema.is_empty() {
            return Err(Error::InvalidQuery(
                "a table must contain at least one column".to_owned(),
            ));
        }
        let mut column_names = HashSet::with_capacity(schema.len());
        for field in &schema {
            if is_reserved_column_name(&field.name) {
                return Err(Error::ReservedIdentifier {
                    identifier: field.name.clone(),
                    context: "column name".to_owned(),
                });
            }
            if !column_names.insert(field.name.to_ascii_lowercase()) {
                return Err(Error::DuplicateColumn(field.name.clone()));
            }
        }
        let columns = schema
            .iter()
            .map(|field| Column::new(field.data_type))
            .collect();
        Ok(Self {
            name,
            schema,
            columns,
            row_count: 0,
        })
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn schema(&self) -> &[ColumnDef] {
        &self.schema
    }

    #[must_use]
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    #[must_use]
    pub fn row_count(&self) -> usize {
        self.row_count
    }

    pub fn column_index(&self, name: &str) -> Result<usize> {
        self.schema
            .iter()
            .position(|field| field.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| Error::ColumnNotFound {
                table: self.name.clone(),
                column: name.to_owned(),
            })
    }

    /// Validates a row and inserts it through the columnar batch path.
    pub fn insert_row(&mut self, row: Vec<Value>) -> Result<()> {
        if row.len() != self.schema.len() {
            return Err(Error::RowLength {
                table: self.name.clone(),
                expected: self.schema.len(),
                actual: row.len(),
            });
        }
        let columns = row
            .into_iter()
            .map(|value| match value {
                Value::Int64(value) => Column::Int64(vec![value]),
                Value::Float64(value) => Column::Float64(vec![value]),
                Value::Bool(value) => Column::Bool(vec![value]),
                Value::String(value) => Column::String(vec![value]),
            })
            .collect();
        self.insert_batch(ColumnBatch::new(columns)).map(|_| ())
    }

    /// Validates and atomically appends an owned set of typed columns.
    pub fn insert_batch(&mut self, batch: ColumnBatch) -> Result<usize> {
        let batch_row_count = self.validate_batch(&batch)?;
        let new_row_count = self.row_count.checked_add(batch_row_count).ok_or_else(|| {
            Error::NumericOverflow(format!("row count for table '{}'", self.name))
        })?;

        for column in &mut self.columns {
            column.reserve(batch_row_count);
        }
        for (column, incoming) in self.columns.iter_mut().zip(batch.columns) {
            column.append(incoming);
        }
        self.row_count = new_row_count;
        Ok(batch_row_count)
    }

    fn validate_batch(&self, batch: &ColumnBatch) -> Result<usize> {
        if batch.columns.len() != self.schema.len() {
            return Err(Error::BatchWidth {
                table: self.name.clone(),
                expected: self.schema.len(),
                actual: batch.columns.len(),
            });
        }

        let row_count = batch.columns.first().map_or(0, Column::len);
        for (field, column) in self.schema.iter().zip(&batch.columns) {
            if field.data_type != column.data_type() {
                return Err(Error::TypeMismatch {
                    context: format!("column '{}.{}'", self.name, field.name),
                    expected: field.data_type.to_string(),
                    actual: column.data_type().to_string(),
                });
            }
            if column.len() != row_count {
                return Err(Error::ColumnLength {
                    table: self.name.clone(),
                    column: field.name.clone(),
                    expected: row_count,
                    actual: column.len(),
                });
            }
            if column.contains_non_finite_float() {
                return Err(Error::InvalidQuery(format!(
                    "column '{}.{}' cannot store a non-finite Float64",
                    self.name, field.name
                )));
            }
        }
        Ok(row_count)
    }

    pub(crate) fn columnarize_rows(&self, rows: Vec<Vec<Value>>) -> Result<ColumnBatch> {
        let mut columns = self
            .schema
            .iter()
            .map(|field| Column::with_capacity(field.data_type, rows.len()))
            .collect::<Vec<_>>();

        for row in rows {
            if row.len() != self.schema.len() {
                return Err(Error::RowLength {
                    table: self.name.clone(),
                    expected: self.schema.len(),
                    actual: row.len(),
                });
            }
            for ((field, column), value) in self.schema.iter().zip(&mut columns).zip(row) {
                if field.data_type != value.data_type() {
                    return Err(Error::TypeMismatch {
                        context: format!("column '{}.{}'", self.name, field.name),
                        expected: field.data_type.to_string(),
                        actual: value.data_type().to_string(),
                    });
                }
                column.push(value);
            }
        }

        Ok(ColumnBatch::new(columns))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_table() -> Table {
        Table::new(
            "events".to_owned(),
            vec![
                ColumnDef {
                    name: "id".to_owned(),
                    data_type: DataType::Int64,
                },
                ColumnDef {
                    name: "label".to_owned(),
                    data_type: DataType::String,
                },
            ],
        )
        .expect("valid schema")
    }

    #[test]
    fn stores_values_in_typed_columns() {
        let mut table = test_table();
        table
            .insert_row(vec![Value::Int64(7), Value::String("ok".to_owned())])
            .expect("valid row");

        assert!(matches!(&table.columns()[0], Column::Int64(v) if v == &[7]));
        assert!(matches!(&table.columns()[1], Column::String(v) if v == &["ok"]));
    }

    #[test]
    fn rejected_rows_do_not_partially_mutate_columns() {
        let mut table = test_table();
        let error = table
            .insert_row(vec![Value::Int64(7), Value::Bool(true)])
            .expect_err("wrong type");

        assert!(matches!(error, Error::TypeMismatch { .. }));
        assert_eq!(table.row_count(), 0);
        assert!(table.columns().iter().all(Column::is_empty));
    }

    #[test]
    fn row_count_overflow_is_rejected_before_columns_change() {
        let mut table = test_table();
        table.row_count = usize::MAX;

        let error = table
            .insert_batch(ColumnBatch::new(vec![
                Column::Int64(vec![1]),
                Column::String(vec!["one".to_owned()]),
            ]))
            .expect_err("row count overflows");

        assert!(
            matches!(error, Error::NumericOverflow(operation) if operation.contains("row count"))
        );
        assert_eq!(table.row_count(), usize::MAX);
        assert!(table.columns().iter().all(Column::is_empty));
    }
}
