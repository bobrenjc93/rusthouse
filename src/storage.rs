use std::collections::{HashMap, HashSet};
use std::sync::Arc;

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

/// The number of distinct strings addressable by a `u32` dictionary code.
pub const LOW_CARDINALITY_MAX_DICTIONARY_ENTRIES: u64 = u32::MAX as u64 + 1;

/// Dictionary-encoded string storage with stable, first-seen codes.
#[derive(Debug, Clone)]
pub struct LowCardinalityStringColumn {
    dictionary: Vec<Arc<str>>,
    codes: Vec<u32>,
    lookup: HashMap<Arc<str>, u32>,
    max_code: u32,
}

impl LowCardinalityStringColumn {
    fn new() -> Self {
        Self::with_max_code(u32::MAX)
    }

    fn with_max_code(max_code: u32) -> Self {
        Self {
            dictionary: Vec::new(),
            codes: Vec::new(),
            lookup: HashMap::new(),
            max_code,
        }
    }

    /// Returns dictionary values in deterministic first-seen order.
    pub fn dictionary(&self) -> impl ExactSizeIterator<Item = &str> {
        self.dictionary.iter().map(|value| value.as_ref())
    }

    #[must_use]
    pub fn dictionary_len(&self) -> usize {
        self.dictionary.len()
    }

    /// Returns the compact dictionary code for each row.
    #[must_use]
    pub fn codes(&self) -> &[u32] {
        &self.codes
    }

    fn len(&self) -> usize {
        self.codes.len()
    }

    fn value(&self, row: usize) -> &str {
        self.dictionary[self.codes[row] as usize].as_ref()
    }

    fn contains(&self, value: &str) -> bool {
        self.lookup.contains_key(value)
    }

    fn can_add(&self, additional_entries: usize) -> bool {
        u64::try_from(self.dictionary.len())
            .ok()
            .and_then(|current| {
                u64::try_from(additional_entries)
                    .ok()
                    .and_then(|additional| current.checked_add(additional))
            })
            .is_some_and(|entries| entries <= u64::from(self.max_code) + 1)
    }

    fn maximum_entries(&self) -> u64 {
        u64::from(self.max_code) + 1
    }

    fn push(&mut self, value: String) {
        let code = if let Some(code) = self.lookup.get(value.as_str()) {
            *code
        } else {
            let code = u32::try_from(self.dictionary.len())
                .expect("LowCardinality dictionary growth is preflighted");
            assert!(
                code <= self.max_code,
                "LowCardinality dictionary growth is preflighted"
            );
            let value = Arc::<str>::from(value);
            self.dictionary.push(Arc::clone(&value));
            self.lookup.insert(value, code);
            code
        };
        self.codes.push(code);
    }
}

/// A physical column. Each variant owns a contiguous vector of one Rust type.
#[derive(Debug, Clone)]
pub enum Column {
    Int64(Vec<i64>),
    Float64(Vec<f64>),
    Bool(Vec<bool>),
    String(Vec<String>),
    LowCardinalityString(LowCardinalityStringColumn),
}

impl Column {
    #[must_use]
    pub fn new(data_type: DataType) -> Self {
        match data_type {
            DataType::Int64 => Self::Int64(Vec::new()),
            DataType::Float64 => Self::Float64(Vec::new()),
            DataType::Bool => Self::Bool(Vec::new()),
            DataType::String => Self::String(Vec::new()),
            DataType::LowCardinalityString => {
                Self::LowCardinalityString(LowCardinalityStringColumn::new())
            }
        }
    }

    #[must_use]
    pub fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
            Self::LowCardinalityString(_) => DataType::LowCardinalityString,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Int64(values) => values.len(),
            Self::Float64(values) => values.len(),
            Self::Bool(values) => values.len(),
            Self::String(values) => values.len(),
            Self::LowCardinalityString(values) => values.len(),
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
            Self::LowCardinalityString(values) => ValueRef::String(values.value(row)),
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
            (Self::LowCardinalityString(values), Value::String(value)) => values.push(value),
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

    fn validate_row_types(&self, row: &[Value]) -> Result<()> {
        if row.len() != self.schema.len() {
            return Err(Error::RowLength {
                table: self.name.clone(),
                expected: self.schema.len(),
                actual: row.len(),
            });
        }

        for (field, value) in self.schema.iter().zip(row) {
            if !field.data_type.accepts(value.data_type()) {
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

    fn validate_rows(&self, rows: &[Vec<Value>]) -> Result<()> {
        for row in rows {
            self.validate_row_types(row)?;
        }

        for (column_index, column) in self.columns.iter().enumerate() {
            let Column::LowCardinalityString(column) = column else {
                continue;
            };
            let mut new_values = HashSet::new();
            for row in rows {
                let Value::String(value) = &row[column_index] else {
                    unreachable!("row types are validated")
                };
                if !column.contains(value) {
                    new_values.insert(value.as_str());
                }
            }
            if !column.can_add(new_values.len()) {
                return Err(Error::DictionaryLimit {
                    table: self.name.clone(),
                    column: self.schema[column_index].name.clone(),
                    maximum_entries: column.maximum_entries(),
                });
            }
        }

        Ok(())
    }

    /// Validates the complete row before appending one value to each column.
    pub fn insert_row(&mut self, row: Vec<Value>) -> Result<()> {
        self.insert_rows(vec![row])?;
        Ok(())
    }

    /// Validates a complete batch before mutating columns or dictionaries.
    pub fn insert_rows(&mut self, rows: Vec<Vec<Value>>) -> Result<()> {
        let row_count = self
            .row_count
            .checked_add(rows.len())
            .ok_or_else(|| Error::NumericOverflow("table row count".to_owned()))?;
        self.validate_rows(&rows)?;
        for row in rows {
            for (column, value) in self.columns.iter_mut().zip(row) {
                column.push(value);
            }
        }
        self.row_count = row_count;
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
    fn low_cardinality_uses_first_seen_dictionary_codes() {
        let mut table = Table::new(
            "events".to_owned(),
            vec![ColumnDef {
                name: "label".to_owned(),
                data_type: DataType::LowCardinalityString,
            }],
        )
        .expect("valid schema");
        table
            .insert_rows(vec![
                vec![Value::String("zeta".to_owned())],
                vec![Value::String("alpha".to_owned())],
                vec![Value::String("zeta".to_owned())],
                vec![Value::String("beta".to_owned())],
                vec![Value::String("alpha".to_owned())],
            ])
            .expect("valid rows");

        let Column::LowCardinalityString(column) = &table.columns()[0] else {
            panic!("expected LowCardinality storage")
        };
        assert_eq!(
            column.dictionary().collect::<Vec<_>>(),
            ["zeta", "alpha", "beta"]
        );
        assert_eq!(column.codes(), &[0, 1, 0, 2, 1]);
    }

    #[test]
    fn dictionary_limit_failure_is_atomic_across_a_batch() {
        let mut table = Table::new(
            "events".to_owned(),
            vec![
                ColumnDef {
                    name: "id".to_owned(),
                    data_type: DataType::Int64,
                },
                ColumnDef {
                    name: "label".to_owned(),
                    data_type: DataType::LowCardinalityString,
                },
            ],
        )
        .expect("valid schema");
        table.columns[1] =
            Column::LowCardinalityString(LowCardinalityStringColumn::with_max_code(1));

        let error = table
            .insert_rows(vec![
                vec![Value::Int64(1), Value::String("a".to_owned())],
                vec![Value::Int64(2), Value::String("b".to_owned())],
                vec![Value::Int64(3), Value::String("c".to_owned())],
            ])
            .expect_err("three values exceed two available codes");

        assert!(matches!(
            error,
            Error::DictionaryLimit {
                maximum_entries: 2,
                ..
            }
        ));
        assert_eq!(table.row_count(), 0);
        assert!(table.columns().iter().all(Column::is_empty));
        let Column::LowCardinalityString(column) = &table.columns()[1] else {
            panic!("expected LowCardinality storage")
        };
        assert_eq!(column.dictionary_len(), 0);
    }
}
