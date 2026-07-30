use std::collections::HashMap;

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

    fn with_default(data_type: DataType, len: usize) -> Result<Self> {
        fn filled<T: Clone>(value: T, len: usize) -> Result<Vec<T>> {
            let mut values = Vec::new();
            values
                .try_reserve_exact(len)
                .map_err(|_| Error::Capacity(format!("a default-filled column with {len} rows")))?;
            values.resize(len, value);
            Ok(values)
        }

        match data_type {
            DataType::Int64 => filled(0, len).map(Self::Int64),
            DataType::Float64 => filled(0.0, len).map(Self::Float64),
            DataType::Bool => filled(false, len).map(Self::Bool),
            DataType::String => filled(String::new(), len).map(Self::String),
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
    column_indexes: HashMap<String, usize>,
    row_count: usize,
}

impl Table {
    pub fn new(name: String, schema: Vec<ColumnDef>) -> Result<Self> {
        if schema.is_empty() {
            return Err(Error::InvalidQuery(
                "a table must contain at least one column".to_owned(),
            ));
        }
        let mut column_indexes = HashMap::with_capacity(schema.len());
        for (index, field) in schema.iter().enumerate() {
            if is_reserved_column_name(&field.name) {
                return Err(Error::ReservedIdentifier {
                    identifier: field.name.clone(),
                    context: "column name".to_owned(),
                });
            }
            if column_indexes
                .insert(normalize(&field.name), index)
                .is_some()
            {
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
            column_indexes,
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
        self.column_indexes
            .get(&normalize(name))
            .copied()
            .ok_or_else(|| Error::ColumnNotFound {
                table: self.name.clone(),
                column: name.to_owned(),
            })
    }

    /// Adds a default-filled physical column and its schema entry as one operation.
    pub fn add_column(&mut self, field: ColumnDef, after: Option<&str>) -> Result<()> {
        if is_reserved_column_name(&field.name) {
            return Err(Error::ReservedIdentifier {
                identifier: field.name,
                context: "column name".to_owned(),
            });
        }

        let key = normalize(&field.name);
        if self.column_indexes.contains_key(&key) {
            return Err(Error::DuplicateColumn(field.name));
        }

        let insertion_index = match after {
            Some(target) => self.column_index(target)? + 1,
            None => self.schema.len(),
        };
        let column = Column::with_default(field.data_type, self.row_count)?;

        self.schema
            .try_reserve(1)
            .map_err(|_| Error::Capacity("table schema".to_owned()))?;
        self.columns
            .try_reserve(1)
            .map_err(|_| Error::Capacity("table columns".to_owned()))?;
        self.column_indexes
            .try_reserve(1)
            .map_err(|_| Error::Capacity("column-name index".to_owned()))?;

        for index in self.column_indexes.values_mut() {
            if *index >= insertion_index {
                *index += 1;
            }
        }
        self.schema.insert(insertion_index, field);
        self.columns.insert(insertion_index, column);
        self.column_indexes.insert(key, insertion_index);

        debug_assert_eq!(self.schema.len(), self.columns.len());
        debug_assert!(
            self.columns
                .iter()
                .all(|column| column.len() == self.row_count)
        );
        Ok(())
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
        for (column, value) in self.columns.iter_mut().zip(row) {
            column.push(value);
        }
        self.row_count += 1;
        Ok(())
    }
}

fn normalize(identifier: &str) -> String {
    identifier.to_ascii_lowercase()
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
    fn added_columns_backfill_and_shift_name_indexes() {
        let mut table = test_table();
        table
            .insert_row(vec![Value::Int64(7), Value::String("ok".to_owned())])
            .expect("valid row");

        table
            .add_column(
                ColumnDef {
                    name: "active".to_owned(),
                    data_type: DataType::Bool,
                },
                Some("ID"),
            )
            .expect("add column");

        assert_eq!(table.column_index("id"), Ok(0));
        assert_eq!(table.column_index("ACTIVE"), Ok(1));
        assert_eq!(table.column_index("label"), Ok(2));
        assert!(matches!(&table.columns()[1], Column::Bool(values) if values == &[false]));
    }

    #[test]
    fn failed_add_column_leaves_schema_and_rows_unchanged() {
        let mut table = test_table();
        table
            .insert_row(vec![Value::Int64(7), Value::String("ok".to_owned())])
            .expect("valid row");
        let schema = table.schema().to_vec();

        let duplicate = table
            .add_column(
                ColumnDef {
                    name: "ID".to_owned(),
                    data_type: DataType::Bool,
                },
                None,
            )
            .expect_err("duplicate name");
        assert_eq!(duplicate, Error::DuplicateColumn("ID".to_owned()));

        let missing_target = table
            .add_column(
                ColumnDef {
                    name: "active".to_owned(),
                    data_type: DataType::Bool,
                },
                Some("missing"),
            )
            .expect_err("missing AFTER target");
        assert_eq!(
            missing_target,
            Error::ColumnNotFound {
                table: "events".to_owned(),
                column: "missing".to_owned(),
            }
        );

        assert_eq!(table.schema(), schema);
        assert_eq!(table.row_count(), 1);
        assert_eq!(table.columns()[0].value(0), Value::Int64(7));
        assert_eq!(table.columns()[1].value(0), Value::String("ok".to_owned()));
        assert!(table.column_index("active").is_err());
    }
}
