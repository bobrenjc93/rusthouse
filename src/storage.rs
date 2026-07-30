use std::cmp::Ordering;
use std::collections::HashMap;
use std::ops::Range;

use crate::error::{Error, Result};
use crate::value::{DataType, Value, ValueRef};

/// The number of rows represented by one sparse primary-key mark.
pub const DEFAULT_MARK_GRANULARITY: usize = 64;

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

/// A sampled primary-key tuple and its row offset within an immutable part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SparseMark {
    row: usize,
    key: Vec<Value>,
}

impl SparseMark {
    #[must_use]
    pub fn row(&self) -> usize {
        self.row
    }

    #[must_use]
    pub fn key(&self) -> &[Value] {
        &self.key
    }
}

/// One immutable, independently sorted collection of typed columns.
#[derive(Debug, Clone)]
pub struct Part {
    columns: Vec<Column>,
    marks: Vec<SparseMark>,
    row_count: usize,
}

impl Part {
    #[must_use]
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    #[must_use]
    pub fn marks(&self) -> &[SparseMark] {
        &self.marks
    }

    #[must_use]
    pub fn row_count(&self) -> usize {
        self.row_count
    }

    fn candidate_range(&self, order_key: &[usize], key_range: &KeyRange) -> Range<usize> {
        let first_mark_not_before = self
            .marks
            .partition_point(|mark| key_before(key_range, |position| mark.key[position].as_ref()));
        let lower_start = first_mark_not_before
            .checked_sub(1)
            .map_or(0, |mark| self.marks[mark].row);
        let lower_end = self
            .marks
            .get(first_mark_not_before)
            .map_or(self.row_count, |mark| (mark.row + 1).min(self.row_count));
        let start = partition_point(lower_start, lower_end, |row| {
            key_before(key_range, |position| {
                self.columns[order_key[position]].value_ref(row)
            })
        });

        let first_mark_after = self
            .marks
            .partition_point(|mark| !key_after(key_range, |position| mark.key[position].as_ref()));
        let upper_start = first_mark_after
            .checked_sub(1)
            .map_or(0, |mark| self.marks[mark].row)
            .max(start);
        let upper_end = self
            .marks
            .get(first_mark_after)
            .map_or(self.row_count, |mark| (mark.row + 1).min(self.row_count));
        let end = partition_point(upper_start, upper_end, |row| {
            !key_after(key_range, |position| {
                self.columns[order_key[position]].value_ref(row)
            })
        });

        start..end.max(start)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RowId {
    pub part: usize,
    pub row: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct PartScan {
    pub part: usize,
    pub rows: Range<usize>,
}

#[derive(Debug, Clone)]
pub(crate) struct KeyBound {
    pub value: Value,
    pub inclusive: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct KeyRange {
    pub equalities: Vec<Value>,
    pub lower: Option<KeyBound>,
    pub upper: Option<KeyBound>,
}

/// A table is a collection of immutable sorted columnar parts.
#[derive(Debug, Clone)]
pub struct Table {
    name: String,
    schema: Vec<ColumnDef>,
    column_indexes: HashMap<String, usize>,
    order_key: Vec<usize>,
    parts: Vec<Part>,
    row_count: usize,
}

impl Table {
    pub fn new(name: String, schema: Vec<ColumnDef>) -> Result<Self> {
        Self::new_ordered(name, schema, Vec::new())
    }

    pub fn new_ordered(
        name: String,
        schema: Vec<ColumnDef>,
        order_by: Vec<String>,
    ) -> Result<Self> {
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
                .insert(field.name.to_ascii_lowercase(), index)
                .is_some()
            {
                return Err(Error::DuplicateColumn(field.name.clone()));
            }
        }

        let mut order_key = Vec::with_capacity(order_by.len());
        let mut selected = vec![false; schema.len()];
        for key in order_by {
            let Some(&column) = column_indexes.get(&key.to_ascii_lowercase()) else {
                return Err(Error::ColumnNotFound {
                    table: name,
                    column: key,
                });
            };
            if selected[column] {
                return Err(Error::InvalidQuery(format!(
                    "ORDER BY key '{}' is listed more than once",
                    schema[column].name
                )));
            }
            selected[column] = true;
            order_key.push(column);
        }

        Ok(Self {
            name,
            schema,
            column_indexes,
            order_key,
            parts: Vec::new(),
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
    pub fn order_key(&self) -> &[usize] {
        &self.order_key
    }

    #[must_use]
    pub fn parts(&self) -> &[Part] {
        &self.parts
    }

    #[must_use]
    pub fn row_count(&self) -> usize {
        self.row_count
    }

    pub fn column_index(&self, name: &str) -> Result<usize> {
        self.column_indexes
            .get(&name.to_ascii_lowercase())
            .copied()
            .ok_or_else(|| Error::ColumnNotFound {
                table: self.name.clone(),
                column: name.to_owned(),
            })
    }

    /// Checks a row without mutating table state.
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

    /// Validates and publishes a complete immutable part atomically.
    pub fn insert_rows(&mut self, mut rows: Vec<Vec<Value>>) -> Result<()> {
        for row in &rows {
            self.validate_row(row)?;
        }
        if rows.is_empty() {
            return Ok(());
        }

        if !self.order_key.is_empty() {
            rows.sort_by(|left, right| {
                self.order_key
                    .iter()
                    .map(|column| left[*column].cmp(&right[*column]))
                    .find(|ordering| *ordering != Ordering::Equal)
                    .unwrap_or(Ordering::Equal)
            });
        }

        let row_count = rows.len();
        let mut columns = self
            .schema
            .iter()
            .map(|field| Column::new(field.data_type))
            .collect::<Vec<_>>();
        let mut marks = Vec::new();
        for (row_index, row) in rows.into_iter().enumerate() {
            if !self.order_key.is_empty() && row_index % DEFAULT_MARK_GRANULARITY == 0 {
                marks.push(SparseMark {
                    row: row_index,
                    key: self
                        .order_key
                        .iter()
                        .map(|column| row[*column].clone())
                        .collect(),
                });
            }
            for (column, value) in columns.iter_mut().zip(row) {
                column.push(value);
            }
        }

        self.parts.push(Part {
            columns,
            marks,
            row_count,
        });
        self.row_count += row_count;
        Ok(())
    }

    pub fn insert_row(&mut self, row: Vec<Value>) -> Result<()> {
        self.insert_rows(vec![row])
    }

    pub(crate) fn value_ref(&self, row: RowId, column: usize) -> ValueRef<'_> {
        self.parts[row.part].columns[column].value_ref(row.row)
    }

    pub(crate) fn value(&self, row: RowId, column: usize) -> Value {
        self.value_ref(row, column).to_owned()
    }

    pub(crate) fn compare_rows(&self, left: RowId, right: RowId, columns: &[usize]) -> Ordering {
        columns
            .iter()
            .map(|column| {
                self.value_ref(left, *column)
                    .cmp(&self.value_ref(right, *column))
            })
            .find(|ordering| *ordering != Ordering::Equal)
            .unwrap_or_else(|| left.cmp(&right))
    }

    pub(crate) fn scan_parts(&self, key_range: Option<&KeyRange>) -> Vec<PartScan> {
        self.parts
            .iter()
            .enumerate()
            .map(|(part, data)| PartScan {
                part,
                rows: if self.order_key.is_empty() {
                    0..data.row_count
                } else if let Some(key_range) = key_range {
                    data.candidate_range(&self.order_key, key_range)
                } else {
                    0..data.row_count
                },
            })
            .collect()
    }
}

fn key_before<'a>(range: &KeyRange, mut value: impl FnMut(usize) -> ValueRef<'a>) -> bool {
    for (position, expected) in range.equalities.iter().enumerate() {
        match value(position)
            .sql_cmp(expected.as_ref())
            .expect("primary-key predicate types are validated")
        {
            Ordering::Less => return true,
            Ordering::Greater => return false,
            Ordering::Equal => {}
        }
    }
    range.lower.as_ref().is_some_and(|bound| {
        let ordering = value(range.equalities.len())
            .sql_cmp(bound.value.as_ref())
            .expect("primary-key predicate types are validated");
        ordering == Ordering::Less || (ordering == Ordering::Equal && !bound.inclusive)
    })
}

fn key_after<'a>(range: &KeyRange, mut value: impl FnMut(usize) -> ValueRef<'a>) -> bool {
    for (position, expected) in range.equalities.iter().enumerate() {
        match value(position)
            .sql_cmp(expected.as_ref())
            .expect("primary-key predicate types are validated")
        {
            Ordering::Less => return false,
            Ordering::Greater => return true,
            Ordering::Equal => {}
        }
    }
    range.upper.as_ref().is_some_and(|bound| {
        let ordering = value(range.equalities.len())
            .sql_cmp(bound.value.as_ref())
            .expect("primary-key predicate types are validated");
        ordering == Ordering::Greater || (ordering == Ordering::Equal && !bound.inclusive)
    })
}

fn partition_point(
    mut start: usize,
    mut end: usize,
    mut predicate: impl FnMut(usize) -> bool,
) -> usize {
    while start < end {
        let middle = start + (end - start) / 2;
        if predicate(middle) {
            start = middle + 1;
        } else {
            end = middle;
        }
    }
    start
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
    fn stores_values_in_typed_part_columns() {
        let mut table = test_table();
        table
            .insert_row(vec![Value::Int64(7), Value::String("ok".to_owned())])
            .expect("valid row");

        assert!(matches!(&table.parts()[0].columns()[0], Column::Int64(v) if v == &[7]));
        assert!(matches!(&table.parts()[0].columns()[1], Column::String(v) if v == &["ok"]));
    }

    #[test]
    fn rejected_rows_do_not_publish_partial_parts() {
        let mut table = test_table();
        let error = table
            .insert_rows(vec![
                vec![Value::Int64(7), Value::String("ok".to_owned())],
                vec![Value::Int64(8), Value::Bool(true)],
            ])
            .expect_err("wrong type");

        assert!(matches!(error, Error::TypeMismatch { .. }));
        assert_eq!(table.row_count(), 0);
        assert!(table.parts().is_empty());
    }

    #[test]
    fn ordered_insert_builds_sorted_columns_and_sparse_marks() {
        let mut table = Table::new_ordered(
            "events".to_owned(),
            test_table().schema().to_vec(),
            vec!["id".to_owned()],
        )
        .expect("ordered table");
        let rows = (0..130)
            .rev()
            .map(|id| vec![Value::Int64(id), Value::String(format!("row {id}"))])
            .collect();
        table.insert_rows(rows).expect("insert part");

        let part = &table.parts()[0];
        assert!(
            matches!(&part.columns()[0], Column::Int64(values) if values[0] == 0 && values[129] == 129)
        );
        assert_eq!(
            part.marks().iter().map(SparseMark::row).collect::<Vec<_>>(),
            [0, 64, 128]
        );
    }
}
