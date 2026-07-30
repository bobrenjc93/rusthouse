use std::collections::{HashMap, HashSet};
use std::mem::size_of;
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

/// A physical column. Each variant owns a contiguous vector of one Rust type.
#[derive(Debug, Clone)]
pub enum Column {
    Int64(Vec<i64>),
    Float64(Vec<f64>),
    Bool(Vec<bool>),
    String(Vec<String>),
    LowCardinalityString(LowCardinalityStringColumn),
}

const MAX_DICTIONARY_CARDINALITY: u64 = u32::MAX as u64 + 1;

/// Dictionary storage for a `LowCardinality(String)` column.
///
/// Each distinct string has one shared allocation and a checked `u32` ID.
/// Rows store only those IDs; dictionary internals stay private so an ID can
/// never refer to a missing string.
#[derive(Debug, Clone)]
pub struct LowCardinalityStringColumn {
    dictionary: Vec<Arc<str>>,
    lookup: HashMap<Arc<str>, u32>,
    ids: Vec<u32>,
    max_cardinality: u64,
}

impl Default for LowCardinalityStringColumn {
    fn default() -> Self {
        Self {
            dictionary: Vec::new(),
            lookup: HashMap::new(),
            ids: Vec::new(),
            max_cardinality: MAX_DICTIONARY_CARDINALITY,
        }
    }
}

impl LowCardinalityStringColumn {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    #[must_use]
    pub fn cardinality(&self) -> usize {
        self.dictionary.len()
    }

    /// The compact physical row IDs, useful for storage inspection.
    #[must_use]
    pub fn ids(&self) -> &[u32] {
        &self.ids
    }

    /// Returns distinct values in stable ID order.
    pub fn dictionary_values(&self) -> impl ExactSizeIterator<Item = &str> {
        self.dictionary.iter().map(AsRef::as_ref)
    }

    #[must_use]
    pub fn value(&self, row: usize) -> &str {
        let id = self.ids[row] as usize;
        &self.dictionary[id]
    }

    /// Estimated bytes allocated by IDs, dictionary values, and the lookup.
    ///
    /// `HashMap` does not expose its allocation layout, so its reserved entry
    /// storage is estimated from capacity. String bytes and vector buffers are
    /// accounted from their actual lengths and capacities.
    #[must_use]
    pub fn allocated_bytes(&self) -> usize {
        let dictionary_values = self
            .dictionary
            .iter()
            .map(|value| value.len().saturating_add(2 * size_of::<usize>()))
            .sum::<usize>();
        self.ids
            .capacity()
            .saturating_mul(size_of::<u32>())
            .saturating_add(
                self.dictionary
                    .capacity()
                    .saturating_mul(size_of::<Arc<str>>()),
            )
            .saturating_add(dictionary_values)
            .saturating_add(
                self.lookup
                    .capacity()
                    .saturating_mul(size_of::<(Arc<str>, u32)>() + 1),
            )
    }

    fn ensure_can_append<'a>(
        &self,
        values: impl Iterator<Item = &'a str>,
        column: &str,
    ) -> Result<()> {
        let additional = values
            .filter(|value| !self.lookup.contains_key(*value))
            .collect::<HashSet<_>>()
            .len() as u64;
        let requested = (self.dictionary.len() as u64)
            .checked_add(additional)
            .ok_or_else(|| Error::DictionaryCardinalityExceeded {
                column: column.to_owned(),
                maximum: self.max_cardinality,
            })?;
        if requested > self.max_cardinality {
            return Err(Error::DictionaryCardinalityExceeded {
                column: column.to_owned(),
                maximum: self.max_cardinality,
            });
        }
        Ok(())
    }

    fn push(&mut self, value: String) {
        let id = if let Some(id) = self.lookup.get(value.as_str()) {
            *id
        } else {
            let id = checked_dictionary_id(self.dictionary.len())
                .expect("dictionary cardinality is checked before insertion");
            let value = Arc::<str>::from(value);
            self.dictionary.push(Arc::clone(&value));
            self.lookup.insert(value, id);
            id
        };
        self.ids.push(id);
    }
}

fn checked_dictionary_id(cardinality: usize) -> std::result::Result<u32, ()> {
    u32::try_from(cardinality).map_err(|_| ())
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

    /// Estimated heap bytes allocated by this column's physical storage.
    #[must_use]
    pub fn allocated_bytes(&self) -> usize {
        match self {
            Self::Int64(values) => values.capacity().saturating_mul(size_of::<i64>()),
            Self::Float64(values) => values.capacity().saturating_mul(size_of::<f64>()),
            Self::Bool(values) => values.capacity().div_ceil(8),
            Self::String(values) => values
                .capacity()
                .saturating_mul(size_of::<String>())
                .saturating_add(values.iter().map(|value| value.capacity()).sum::<usize>()),
            Self::LowCardinalityString(values) => values.allocated_bytes(),
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

    /// Estimated heap bytes allocated by all physical columns.
    #[must_use]
    pub fn allocated_bytes(&self) -> usize {
        self.columns.iter().map(Column::allocated_bytes).sum()
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

    /// Validates the complete row before appending one value to each column.
    pub fn insert_row(&mut self, row: Vec<Value>) -> Result<()> {
        self.insert_rows(vec![row])
    }

    /// Validates a complete batch, including dictionary growth, before append.
    /// Any returned error leaves every column and dictionary unchanged.
    pub fn insert_rows(&mut self, rows: Vec<Vec<Value>>) -> Result<()> {
        for row in &rows {
            self.validate_row(row)?;
        }
        self.row_count
            .checked_add(rows.len())
            .ok_or_else(|| Error::NumericOverflow("table row count".to_owned()))?;

        for (column_index, (field, column)) in self.schema.iter().zip(&self.columns).enumerate() {
            let Column::LowCardinalityString(dictionary) = column else {
                continue;
            };
            dictionary.ensure_can_append(
                rows.iter().map(|row| match &row[column_index] {
                    Value::String(value) => value.as_str(),
                    _ => unreachable!("rows are type checked before dictionary validation"),
                }),
                &format!("{}.{}", self.name, field.name),
            )?;
        }

        let inserted = rows.len();
        for row in rows {
            for (column, value) in self.columns.iter_mut().zip(row) {
                column.push(value);
            }
        }
        self.row_count += inserted;
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
    fn low_cardinality_reuses_checked_u32_ids() {
        let mut column = LowCardinalityStringColumn::new();
        for value in ["west", "east", "west", "north", "east"] {
            column.push(value.to_owned());
        }

        assert_eq!(column.ids(), &[0, 1, 0, 2, 1]);
        assert_eq!(
            column.dictionary_values().collect::<Vec<_>>(),
            ["west", "east", "north"]
        );
        assert_eq!(column.cardinality(), 3);
        assert_eq!(column.value(3), "north");

        assert_eq!(checked_dictionary_id(u32::MAX as usize), Ok(u32::MAX));
        #[cfg(target_pointer_width = "64")]
        assert_eq!(checked_dictionary_id(u32::MAX as usize + 1), Err(()));
    }

    #[test]
    fn dictionary_limit_failure_keeps_batch_atomic() {
        let mut table = Table::new(
            "events".to_owned(),
            vec![
                ColumnDef {
                    name: "id".to_owned(),
                    data_type: DataType::Int64,
                },
                ColumnDef {
                    name: "kind".to_owned(),
                    data_type: DataType::LowCardinalityString,
                },
            ],
        )
        .expect("valid schema");
        let Column::LowCardinalityString(dictionary) = &mut table.columns[1] else {
            panic!("expected dictionary column");
        };
        dictionary.max_cardinality = 2;

        table
            .insert_rows(vec![
                vec![Value::Int64(1), Value::String("one".to_owned())],
                vec![Value::Int64(2), Value::String("two".to_owned())],
            ])
            .expect("the exact cardinality limit is accepted");
        let allocated_before = table.allocated_bytes();

        let error = table
            .insert_rows(vec![
                vec![Value::Int64(3), Value::String("one".to_owned())],
                vec![Value::Int64(4), Value::String("three".to_owned())],
            ])
            .expect_err("a third distinct value exceeds the test limit");
        assert_eq!(
            error,
            Error::DictionaryCardinalityExceeded {
                column: "events.kind".to_owned(),
                maximum: 2,
            }
        );
        assert_eq!(table.row_count(), 2);
        assert_eq!(table.allocated_bytes(), allocated_before);
        assert_eq!(table.columns()[0].value(0), Value::Int64(1));
        assert_eq!(table.columns()[0].value(1), Value::Int64(2));
        let Column::LowCardinalityString(dictionary) = &table.columns()[1] else {
            panic!("expected dictionary column");
        };
        assert_eq!(dictionary.cardinality(), 2);
        assert_eq!(dictionary.ids(), &[0, 1]);
    }

    #[test]
    fn allocated_bytes_show_compression_for_repeated_strings() {
        let repeated = "a repeated analytical dimension value";
        let mut plain = Column::new(DataType::String);
        let mut low = Column::new(DataType::LowCardinalityString);
        for _ in 0..10_000 {
            plain.push(Value::String(repeated.to_owned()));
            low.push(Value::String(repeated.to_owned()));
        }

        let plain_bytes = plain.allocated_bytes();
        let low_bytes = low.allocated_bytes();
        assert!(
            low_bytes * 4 < plain_bytes,
            "expected at least 4x less tracked heap: plain={plain_bytes}, low={low_bytes}"
        );
        assert_eq!(plain.value(9_999), low.value(9_999));
    }
}
