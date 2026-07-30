use std::collections::HashSet;
use std::mem;
use std::ops::Index;

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

/// Contiguous UTF-8 string storage with one checked end offset per value.
///
/// The arena eliminates the allocation and `String` header previously needed
/// for every cell. Values remain available as borrowed `&str` slices.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StringColumn {
    data: Vec<u8>,
    offsets: Vec<u32>,
}

impl StringColumn {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.offsets.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }

    /// Returns a borrowed value without allocating.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&str> {
        let end = usize::try_from(*self.offsets.get(index)?).expect("u32 fits in usize");
        let start = index.checked_sub(1).map_or(0, |previous| {
            usize::try_from(self.offsets[previous]).expect("u32 fits in usize")
        });
        let bytes = &self.data[start..end];

        // SAFETY: data is private and is only extended with complete `String`
        // values. Every stored offset therefore lies on a UTF-8 boundary.
        Some(unsafe { std::str::from_utf8_unchecked(bytes) })
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &str> + DoubleEndedIterator {
        (0..self.len()).map(|index| &self[index])
    }

    /// Bytes occupied by initialized arena data and offsets.
    #[must_use]
    pub fn used_bytes(&self) -> usize {
        self.data
            .len()
            .saturating_add(self.offsets.len().saturating_mul(mem::size_of::<u32>()))
    }

    /// Bytes reserved by the arena and offset allocations.
    #[must_use]
    pub fn allocated_bytes(&self) -> usize {
        self.data.capacity().saturating_add(
            self.offsets
                .capacity()
                .saturating_mul(mem::size_of::<u32>()),
        )
    }

    #[must_use]
    pub fn data_bytes(&self) -> usize {
        self.data.len()
    }

    fn end_offset(&self) -> u32 {
        self.offsets.last().copied().unwrap_or(0)
    }

    fn reserve(&mut self, additional_values: usize, additional_bytes: usize) {
        self.data.reserve(additional_bytes);
        self.offsets.reserve(additional_values);
    }

    fn push(&mut self, value: String) {
        let end = checked_end_offset(self.end_offset(), value.len())
            .expect("string column growth is validated before insertion");
        self.data.extend_from_slice(value.as_bytes());
        self.offsets.push(end);
    }
}

impl Index<usize> for StringColumn {
    type Output = str;

    fn index(&self, index: usize) -> &Self::Output {
        self.get(index)
            .unwrap_or_else(|| panic!("string column index {index} out of bounds"))
    }
}

fn checked_end_offset(current: u32, additional: usize) -> Option<u32> {
    u64::from(current)
        .checked_add(u64::try_from(additional).ok()?)
        .and_then(|end| u32::try_from(end).ok())
}

/// A physical column. Each variant owns contiguous typed storage.
#[derive(Debug, Clone)]
pub enum Column {
    Int64(Vec<i64>),
    Float64(Vec<f64>),
    Bool(Vec<bool>),
    String(StringColumn),
}

impl Column {
    #[must_use]
    pub fn new(data_type: DataType) -> Self {
        match data_type {
            DataType::Int64 => Self::Int64(Vec::new()),
            DataType::Float64 => Self::Float64(Vec::new()),
            DataType::Bool => Self::Bool(Vec::new()),
            DataType::String => Self::String(StringColumn::new()),
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

    fn reserve(&mut self, additional_values: usize, additional_string_bytes: usize) {
        match self {
            Self::Int64(values) => values.reserve(additional_values),
            Self::Float64(values) => values.reserve(additional_values),
            Self::Bool(values) => values.reserve(additional_values),
            Self::String(values) => values.reserve(additional_values, additional_string_bytes),
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

    fn validate_row_values(&self, row: &[Value]) -> Result<()> {
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

    fn validate_rows_and_measure<'a>(
        &self,
        rows: impl IntoIterator<Item = &'a [Value]>,
    ) -> Result<Vec<usize>> {
        let mut string_bytes = vec![0_usize; self.columns.len()];
        for row in rows {
            self.validate_row_values(row)?;
            for (index, (column, value)) in self.columns.iter().zip(row).enumerate() {
                let (Column::String(strings), Value::String(value)) = (column, value) else {
                    continue;
                };
                let additional = string_bytes[index]
                    .checked_add(value.len())
                    .ok_or_else(|| self.string_capacity_error(index))?;
                checked_end_offset(strings.end_offset(), additional)
                    .ok_or_else(|| self.string_capacity_error(index))?;
                string_bytes[index] = additional;
            }
        }
        Ok(string_bytes)
    }

    fn string_capacity_error(&self, column: usize) -> Error {
        Error::StringColumnTooLarge {
            table: self.name.clone(),
            column: self.schema[column].name.clone(),
            maximum_bytes: u32::MAX,
        }
    }

    /// Validates the complete row before appending one value to each column.
    pub fn insert_row(&mut self, row: Vec<Value>) -> Result<()> {
        let string_bytes = self.validate_rows_and_measure(std::iter::once(row.as_slice()))?;
        for (index, column) in self.columns.iter_mut().enumerate() {
            column.reserve(1, string_bytes[index]);
        }
        self.append_row(row);
        Ok(())
    }

    pub(crate) fn insert_rows(&mut self, rows: Vec<Vec<Value>>) -> Result<()> {
        let string_bytes = self.validate_rows_and_measure(rows.iter().map(Vec::as_slice))?;
        for (index, column) in self.columns.iter_mut().enumerate() {
            column.reserve(rows.len(), string_bytes[index]);
        }
        for row in rows {
            self.append_row(row);
        }
        Ok(())
    }

    fn append_row(&mut self, row: Vec<Value>) {
        for (column, value) in self.columns.iter_mut().zip(row) {
            column.push(value);
        }
        self.row_count += 1;
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
        assert!(matches!(&table.columns()[1], Column::String(v) if v.iter().eq(["ok"])));
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
    fn string_column_borrows_unicode_and_empty_values() {
        let mut values = StringColumn::new();
        for value in ["", "café", "東京", "🦀", ""] {
            values.push(value.to_owned());
        }

        assert_eq!(
            values.iter().collect::<Vec<_>>(),
            ["", "café", "東京", "🦀", ""]
        );
        assert_eq!(values.data_bytes(), "café東京🦀".len());
        assert_eq!(values.offsets, [0, 5, 11, 15, 15]);
    }

    #[test]
    fn string_column_offsets_reject_unrepresentable_arenas() {
        assert_eq!(checked_end_offset(0, 0), Some(0));
        assert_eq!(checked_end_offset(u32::MAX - 1, 1), Some(u32::MAX));
        assert_eq!(checked_end_offset(u32::MAX, 0), Some(u32::MAX));
        assert_eq!(checked_end_offset(u32::MAX, 1), None);
        if usize::BITS > u32::BITS {
            assert_eq!(checked_end_offset(0, u32::MAX as usize + 1), None);
        }
    }

    #[test]
    fn cloning_string_columns_copies_arena_and_offsets() {
        let mut original = StringColumn::new();
        original.push("alpha".to_owned());
        original.push("βeta".to_owned());
        let cloned = original.clone();

        original.push("later".to_owned());
        assert_eq!(cloned.iter().collect::<Vec<_>>(), ["alpha", "βeta"]);
        assert_eq!(
            original.iter().collect::<Vec<_>>(),
            ["alpha", "βeta", "later"]
        );
        assert_ne!(cloned.data.as_ptr(), original.data.as_ptr());
        assert_ne!(cloned.offsets.as_ptr(), original.offsets.as_ptr());
    }

    #[test]
    fn string_column_reports_compact_memory_use() {
        let source = (0..1_000)
            .map(|index| format!("value-{index}"))
            .collect::<Vec<_>>();
        let mut values = StringColumn::new();
        values.reserve(source.len(), source.iter().map(String::len).sum());
        for value in &source {
            values.push(value.clone());
        }

        let payload_bytes = source.iter().map(String::len).sum::<usize>();
        let legacy_used = payload_bytes + source.len() * mem::size_of::<String>();
        assert_eq!(values.used_bytes(), payload_bytes + source.len() * 4);
        assert!(values.used_bytes() < legacy_used);
        assert!(values.allocated_bytes() >= values.used_bytes());
    }
}
