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
    Int8(Vec<i8>),
    Int16(Vec<i16>),
    Int32(Vec<i32>),
    Int64(Vec<i64>),
    UInt8(Vec<u8>),
    UInt16(Vec<u16>),
    UInt32(Vec<u32>),
    UInt64(Vec<u64>),
    Float64(Vec<f64>),
    Bool(Vec<bool>),
    String(Vec<String>),
}

impl Column {
    #[must_use]
    pub fn new(data_type: DataType) -> Self {
        match data_type {
            DataType::Int8 => Self::Int8(Vec::new()),
            DataType::Int16 => Self::Int16(Vec::new()),
            DataType::Int32 => Self::Int32(Vec::new()),
            DataType::Int64 => Self::Int64(Vec::new()),
            DataType::UInt8 => Self::UInt8(Vec::new()),
            DataType::UInt16 => Self::UInt16(Vec::new()),
            DataType::UInt32 => Self::UInt32(Vec::new()),
            DataType::UInt64 => Self::UInt64(Vec::new()),
            DataType::Float64 => Self::Float64(Vec::new()),
            DataType::Bool => Self::Bool(Vec::new()),
            DataType::String => Self::String(Vec::new()),
        }
    }

    #[must_use]
    pub fn data_type(&self) -> DataType {
        match self {
            Self::Int8(_) => DataType::Int8,
            Self::Int16(_) => DataType::Int16,
            Self::Int32(_) => DataType::Int32,
            Self::Int64(_) => DataType::Int64,
            Self::UInt8(_) => DataType::UInt8,
            Self::UInt16(_) => DataType::UInt16,
            Self::UInt32(_) => DataType::UInt32,
            Self::UInt64(_) => DataType::UInt64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Int8(values) => values.len(),
            Self::Int16(values) => values.len(),
            Self::Int32(values) => values.len(),
            Self::Int64(values) => values.len(),
            Self::UInt8(values) => values.len(),
            Self::UInt16(values) => values.len(),
            Self::UInt32(values) => values.len(),
            Self::UInt64(values) => values.len(),
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
            Self::Int8(values) => ValueRef::Int8(values[row]),
            Self::Int16(values) => ValueRef::Int16(values[row]),
            Self::Int32(values) => ValueRef::Int32(values[row]),
            Self::Int64(values) => ValueRef::Int64(values[row]),
            Self::UInt8(values) => ValueRef::UInt8(values[row]),
            Self::UInt16(values) => ValueRef::UInt16(values[row]),
            Self::UInt32(values) => ValueRef::UInt32(values[row]),
            Self::UInt64(values) => ValueRef::UInt64(values[row]),
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
            (Self::Int8(values), Value::Int8(value)) => values.push(value),
            (Self::Int16(values), Value::Int16(value)) => values.push(value),
            (Self::Int32(values), Value::Int32(value)) => values.push(value),
            (Self::Int64(values), Value::Int64(value)) => values.push(value),
            (Self::UInt8(values), Value::UInt8(value)) => values.push(value),
            (Self::UInt16(values), Value::UInt16(value)) => values.push(value),
            (Self::UInt32(values), Value::UInt32(value)) => values.push(value),
            (Self::UInt64(values), Value::UInt64(value)) => values.push(value),
            (Self::Float64(values), Value::Float64(value)) => values.push(value),
            (Self::Bool(values), Value::Bool(value)) => values.push(value),
            (Self::String(values), Value::String(value)) => values.push(value),
            _ => unreachable!("values are validated before insertion"),
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

    /// Checks a row without mutating any physical column.
    pub(crate) fn validate_row(&self, row: &[Value]) -> Result<()> {
        if row.len() != self.schema.len() {
            return Err(Error::RowLength {
                table: self.name.clone(),
                expected: self.schema.len(),
                actual: row.len(),
            });
        }

        for (field, value) in self.schema.iter().zip(row) {
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

        Ok(())
    }

    /// Coerces integer literals to the schema's physical widths and validates the row.
    pub(crate) fn prepare_row(&self, row: Vec<Value>) -> Result<Vec<Value>> {
        if row.len() != self.schema.len() {
            return Err(Error::RowLength {
                table: self.name.clone(),
                expected: self.schema.len(),
                actual: row.len(),
            });
        }

        let mut prepared = Vec::with_capacity(row.len());
        for (field, value) in self.schema.iter().zip(row) {
            if field.data_type.is_integer() && value.data_type().is_integer() {
                let rendered = value.as_display_string();
                let coerced = value
                    .checked_coerce_integer(field.data_type)
                    .ok_or_else(|| Error::IntegerOutOfRange {
                        value: rendered,
                        target: field.data_type,
                        context: format!("column '{}.{}'", self.name, field.name),
                    })?;
                prepared.push(coerced);
            } else {
                prepared.push(value);
            }
        }
        self.validate_row(&prepared)?;
        Ok(prepared)
    }

    /// Validates the complete row before appending one value to each column.
    pub fn insert_row(&mut self, row: Vec<Value>) -> Result<()> {
        let row = self.prepare_row(row)?;
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
    fn compact_integers_use_width_specific_physical_vectors() {
        let schema = [
            ("i8", DataType::Int8),
            ("i16", DataType::Int16),
            ("i32", DataType::Int32),
            ("u8", DataType::UInt8),
            ("u16", DataType::UInt16),
            ("u32", DataType::UInt32),
        ]
        .into_iter()
        .map(|(name, data_type)| ColumnDef {
            name: name.to_owned(),
            data_type,
        })
        .collect();
        let mut table = Table::new("compact".to_owned(), schema).expect("valid schema");

        table
            .insert_row(vec![
                Value::Int64(-8),
                Value::Int64(-16),
                Value::Int64(-32),
                Value::Int64(8),
                Value::Int64(16),
                Value::Int64(32),
            ])
            .expect("integer literals are coerced");

        assert!(matches!(&table.columns()[0], Column::Int8(v) if v == &[-8]));
        assert!(matches!(&table.columns()[1], Column::Int16(v) if v == &[-16]));
        assert!(matches!(&table.columns()[2], Column::Int32(v) if v == &[-32]));
        assert!(matches!(&table.columns()[3], Column::UInt8(v) if v == &[8]));
        assert!(matches!(&table.columns()[4], Column::UInt16(v) if v == &[16]));
        assert!(matches!(&table.columns()[5], Column::UInt32(v) if v == &[32]));
    }
}
