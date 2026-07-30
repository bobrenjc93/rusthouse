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
#[derive(Debug, Clone)]
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

    fn truncate(&mut self, length: usize) {
        match self {
            Self::Int64(values) => values.truncate(length),
            Self::Float64(values) => values.truncate(length),
            Self::Bool(values) => values.truncate(length),
            Self::String(values) => values.truncate(length),
        }
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
        Self::build(name, schema, || Ok(()))
    }

    pub(crate) fn new_with_checkpoint(
        name: String,
        schema: Vec<ColumnDef>,
        checkpoint: &mut dyn FnMut() -> Result<()>,
    ) -> Result<Self> {
        Self::build(name, schema, checkpoint)
    }

    fn build(
        name: String,
        schema: Vec<ColumnDef>,
        mut checkpoint: impl FnMut() -> Result<()>,
    ) -> Result<Self> {
        checkpoint()?;
        if schema.is_empty() {
            return Err(Error::InvalidQuery(
                "a table must contain at least one column".to_owned(),
            ));
        }
        let mut column_names = HashSet::with_capacity(schema.len());
        for field in &schema {
            checkpoint()?;
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
        let mut columns = Vec::with_capacity(schema.len());
        for field in &schema {
            checkpoint()?;
            columns.push(Column::new(field.data_type));
        }
        checkpoint()?;
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

    /// Checks a row without mutating any physical column.
    pub(crate) fn validate_row(&self, row: &[Value]) -> Result<()> {
        self.validate_row_inner(row, || Ok(()))
    }

    fn validate_row_with_checkpoint(
        &self,
        row: &[Value],
        checkpoint: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        self.validate_row_inner(row, checkpoint)
    }

    fn validate_row_inner(
        &self,
        row: &[Value],
        mut checkpoint: impl FnMut() -> Result<()>,
    ) -> Result<()> {
        checkpoint()?;
        if row.len() != self.schema.len() {
            return Err(Error::RowLength {
                table: self.name.clone(),
                expected: self.schema.len(),
                actual: row.len(),
            });
        }

        for (field, value) in self.schema.iter().zip(row) {
            checkpoint()?;
            if field.data_type != value.data_type() {
                return Err(Error::TypeMismatch {
                    context: format!("column '{}.{}'", self.name, field.name),
                    expected: field.data_type.to_string(),
                    actual: value.data_type().to_string(),
                });
            }
            if matches!(value, Value::Float64(number) if !number.is_finite()) {
                return Err(Error::InvalidQuery(format!(
                    "column '{}.{}' cannot store a non-finite Float64",
                    self.name, field.name
                )));
            }
        }

        checkpoint()?;
        Ok(())
    }

    pub(crate) fn insert_rows_with_checkpoint(
        &mut self,
        rows: Vec<Vec<Value>>,
        checkpoint: &mut dyn FnMut() -> Result<()>,
    ) -> Result<()> {
        for row in &rows {
            checkpoint()?;
            self.validate_row_with_checkpoint(row, checkpoint)?;
        }
        checkpoint()?;

        let original_row_count = self.row_count;
        for row in rows {
            for (column, value) in self.columns.iter_mut().zip(row) {
                if let Err(error) = checkpoint() {
                    self.truncate_rows(original_row_count);
                    return Err(error);
                }
                column.push(value);
            }
        }
        if let Err(error) = checkpoint() {
            self.truncate_rows(original_row_count);
            return Err(error);
        }
        self.row_count = self.columns[0].len();
        Ok(())
    }

    fn truncate_rows(&mut self, row_count: usize) {
        for column in &mut self.columns {
            column.truncate(row_count);
        }
        self.row_count = row_count;
    }

    /// Validates the complete row before appending one value to each column.
    pub fn insert_row(&mut self, row: Vec<Value>) -> Result<()> {
        self.validate_row(&row)?;
        for (column, value) in self.columns.iter_mut().zip(row) {
            column.push(value);
        }
        self.row_count += 1;
        Ok(())
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
    fn cancelled_batch_append_rolls_back_every_column() {
        let mut table = test_table();
        table
            .insert_row(vec![Value::Int64(1), Value::String("existing".to_owned())])
            .expect("initial row");
        let rows = vec![
            vec![Value::Int64(2), Value::String("first".to_owned())],
            vec![Value::Int64(3), Value::String("second".to_owned())],
        ];
        let mut checkpoints = 0;
        let error = table
            .insert_rows_with_checkpoint(rows, &mut || {
                checkpoints += 1;
                if checkpoints == 13 {
                    Err(Error::ExecutionCancelled)
                } else {
                    Ok(())
                }
            })
            .expect_err("cancellation during append should abort the batch");

        assert_eq!(error, Error::ExecutionCancelled);
        assert_eq!(table.row_count(), 1);
        assert_eq!(table.columns()[0].value(0), Value::Int64(1));
        assert_eq!(
            table.columns()[1].value(0),
            Value::String("existing".to_owned())
        );
        assert!(table.columns().iter().all(|column| column.len() == 1));
    }
}
