use std::collections::HashSet;

use crate::error::{Error, Result};
use crate::value::{DataType, Value, ValueRef};

/// Maximum rows covered by one min/max skip-index block.
pub const BLOCK_SIZE: usize = 1_024;

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

/// Per-block min/max values used to reject blocks that cannot match a predicate.
#[derive(Debug, Clone)]
pub(crate) enum BlockIndex {
    Int64 { min: i64, max: i64 },
    Float64 { min: f64, max: f64 },
    Bool { min: bool, max: bool },
    String { min: String, max: String },
}

impl BlockIndex {
    fn new(value: &Value) -> Self {
        match value {
            Value::Int64(value) => Self::Int64 {
                min: *value,
                max: *value,
            },
            Value::Float64(value) => Self::Float64 {
                min: *value,
                max: *value,
            },
            Value::Bool(value) => Self::Bool {
                min: *value,
                max: *value,
            },
            Value::String(value) => Self::String {
                min: value.clone(),
                max: value.clone(),
            },
        }
    }

    fn update(&mut self, value: &Value) {
        match (self, value) {
            (Self::Int64 { min, max }, Value::Int64(value)) => {
                *min = (*min).min(*value);
                *max = (*max).max(*value);
            }
            (Self::Float64 { min, max }, Value::Float64(value)) => {
                if value.total_cmp(min).is_lt() {
                    *min = *value;
                }
                if value.total_cmp(max).is_gt() {
                    *max = *value;
                }
            }
            (Self::Bool { min, max }, Value::Bool(value)) => {
                *min = (*min).min(*value);
                *max = (*max).max(*value);
            }
            (Self::String { min, max }, Value::String(value)) => {
                if value < min {
                    min.clone_from(value);
                }
                if value > max {
                    max.clone_from(value);
                }
            }
            _ => unreachable!("block indexes have the column's physical type"),
        }
    }

    pub(crate) fn bounds(&self) -> (ValueRef<'_>, ValueRef<'_>) {
        match self {
            Self::Int64 { min, max } => (ValueRef::Int64(*min), ValueRef::Int64(*max)),
            Self::Float64 { min, max } => (ValueRef::Float64(*min), ValueRef::Float64(*max)),
            Self::Bool { min, max } => (ValueRef::Bool(*min), ValueRef::Bool(*max)),
            Self::String { min, max } => (ValueRef::String(min), ValueRef::String(max)),
        }
    }
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
}

/// A table stores one typed vector per schema field.
#[derive(Debug, Clone)]
pub struct Table {
    name: String,
    schema: Vec<ColumnDef>,
    columns: Vec<Column>,
    block_indexes: Vec<Vec<BlockIndex>>,
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
            .collect::<Vec<_>>();
        let block_indexes = vec![Vec::new(); columns.len()];
        Ok(Self {
            name,
            schema,
            columns,
            block_indexes,
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

    #[must_use]
    pub(crate) fn block_count(&self) -> usize {
        self.row_count.div_ceil(BLOCK_SIZE)
    }

    pub(crate) fn block_rows(&self, block: usize) -> std::ops::Range<usize> {
        let start = block * BLOCK_SIZE;
        start..start.saturating_add(BLOCK_SIZE).min(self.row_count)
    }

    pub(crate) fn block_index(&self, column: usize, block: usize) -> &BlockIndex {
        &self.block_indexes[column][block]
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

    /// Validates the complete row before appending one value to each column.
    pub fn insert_row(&mut self, row: Vec<Value>) -> Result<()> {
        self.validate_row(&row)?;
        let starts_block = self.row_count.is_multiple_of(BLOCK_SIZE);
        for (index, (column, value)) in self.columns.iter_mut().zip(row).enumerate() {
            if starts_block {
                self.block_indexes[index].push(BlockIndex::new(&value));
            } else {
                self.block_indexes[index]
                    .last_mut()
                    .expect("a partial block has an index")
                    .update(&value);
            }
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
}
