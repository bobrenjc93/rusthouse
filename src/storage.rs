use std::collections::HashSet;

use crate::error::{Error, Result};
use crate::value::{DataType, Value, ValueRef};

/// Number of adjacent rows summarized by one zone map in every column.
pub const ROW_GROUP_SIZE: usize = 1_024;

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
}

#[derive(Debug, Clone)]
struct MinMax<T> {
    min: T,
    max: T,
}

impl<T: PartialOrd + Copy> MinMax<T> {
    fn update(&mut self, value: T) {
        if value < self.min {
            self.min = value;
        }
        if value > self.max {
            self.max = value;
        }
    }
}

#[derive(Debug, Clone)]
enum ColumnZoneMaps {
    Int64(Vec<MinMax<i64>>),
    Float64(Vec<MinMax<f64>>),
    Bool(Vec<BooleanPresence>),
    String(Vec<MinMax<String>>),
}

#[derive(Debug, Clone, Copy)]
struct BooleanPresence {
    has_false: bool,
    has_true: bool,
}

/// Borrowed typed metadata for one column in one row group.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ZoneMapRef<'a> {
    Int64 { min: i64, max: i64 },
    Float64 { min: f64, max: f64 },
    Bool { has_false: bool, has_true: bool },
    String { min: &'a str, max: &'a str },
}

impl ColumnZoneMaps {
    fn new(data_type: DataType) -> Self {
        match data_type {
            DataType::Int64 => Self::Int64(Vec::new()),
            DataType::Float64 => Self::Float64(Vec::new()),
            DataType::Bool => Self::Bool(Vec::new()),
            DataType::String => Self::String(Vec::new()),
        }
    }

    fn push(&mut self, row: usize, value: &Value) {
        let row_group = row / ROW_GROUP_SIZE;
        match (self, value) {
            (Self::Int64(groups), Value::Int64(value)) => {
                push_min_max(groups, row_group, *value);
            }
            (Self::Float64(groups), Value::Float64(value)) => {
                push_min_max(groups, row_group, *value);
            }
            (Self::Bool(groups), Value::Bool(value)) => {
                if row_group == groups.len() {
                    groups.push(BooleanPresence {
                        has_false: !value,
                        has_true: *value,
                    });
                } else {
                    debug_assert_eq!(row_group + 1, groups.len());
                    let presence = groups.last_mut().expect("current Boolean row group");
                    presence.has_false |= !value;
                    presence.has_true |= *value;
                }
            }
            (Self::String(groups), Value::String(value)) => {
                if row_group == groups.len() {
                    groups.push(MinMax {
                        min: value.clone(),
                        max: value.clone(),
                    });
                } else {
                    debug_assert_eq!(row_group + 1, groups.len());
                    let extrema = groups.last_mut().expect("current String row group");
                    if value < &extrema.min {
                        extrema.min.clone_from(value);
                    }
                    if value > &extrema.max {
                        extrema.max.clone_from(value);
                    }
                }
            }
            _ => unreachable!("values are validated before insertion"),
        }
    }

    fn get(&self, row_group: usize) -> ZoneMapRef<'_> {
        match self {
            Self::Int64(groups) => {
                let extrema = &groups[row_group];
                ZoneMapRef::Int64 {
                    min: extrema.min,
                    max: extrema.max,
                }
            }
            Self::Float64(groups) => {
                let extrema = &groups[row_group];
                ZoneMapRef::Float64 {
                    min: extrema.min,
                    max: extrema.max,
                }
            }
            Self::Bool(groups) => {
                let presence = groups[row_group];
                ZoneMapRef::Bool {
                    has_false: presence.has_false,
                    has_true: presence.has_true,
                }
            }
            Self::String(groups) => {
                let extrema = &groups[row_group];
                ZoneMapRef::String {
                    min: &extrema.min,
                    max: &extrema.max,
                }
            }
        }
    }
}

fn push_min_max<T: PartialOrd + Copy>(groups: &mut Vec<MinMax<T>>, row_group: usize, value: T) {
    if row_group == groups.len() {
        groups.push(MinMax {
            min: value,
            max: value,
        });
    } else {
        debug_assert_eq!(row_group + 1, groups.len());
        groups
            .last_mut()
            .expect("current numeric row group")
            .update(value);
    }
}

/// A table stores one typed vector per schema field.
#[derive(Debug, Clone)]
pub struct Table {
    name: String,
    schema: Vec<ColumnDef>,
    columns: Vec<Column>,
    zone_maps: Vec<ColumnZoneMaps>,
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
        let zone_maps = schema
            .iter()
            .map(|field| ColumnZoneMaps::new(field.data_type))
            .collect();
        Ok(Self {
            name,
            schema,
            columns,
            zone_maps,
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
    pub(crate) fn row_group_count(&self) -> usize {
        self.row_count.div_ceil(ROW_GROUP_SIZE)
    }

    pub(crate) fn row_group_range(&self, row_group: usize) -> std::ops::Range<usize> {
        let start = row_group * ROW_GROUP_SIZE;
        start..(start + ROW_GROUP_SIZE).min(self.row_count)
    }

    pub(crate) fn zone_map(&self, column: usize, row_group: usize) -> ZoneMapRef<'_> {
        self.zone_maps[column].get(row_group)
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
        for ((column, zone_maps), value) in
            self.columns.iter_mut().zip(&mut self.zone_maps).zip(row)
        {
            zone_maps.push(self.row_count, &value);
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
    fn typed_zone_maps_follow_append_row_group_boundaries() {
        let mut table = Table::new(
            "typed".to_owned(),
            vec![
                ColumnDef {
                    name: "integer".to_owned(),
                    data_type: DataType::Int64,
                },
                ColumnDef {
                    name: "float".to_owned(),
                    data_type: DataType::Float64,
                },
                ColumnDef {
                    name: "flag".to_owned(),
                    data_type: DataType::Bool,
                },
                ColumnDef {
                    name: "label".to_owned(),
                    data_type: DataType::String,
                },
            ],
        )
        .expect("valid schema");

        for row in 0..ROW_GROUP_SIZE {
            table
                .insert_row(vec![
                    Value::Int64(row as i64 - 10),
                    Value::Float64(row as f64 / 2.0),
                    Value::Bool(false),
                    Value::String(format!("middle-{row:04}")),
                ])
                .expect("valid row");
        }
        table
            .insert_row(vec![
                Value::Int64(i64::MIN),
                Value::Float64(-1.5),
                Value::Bool(true),
                Value::String("z-last".to_owned()),
            ])
            .expect("first row in appended group");
        table
            .insert_row(vec![
                Value::Int64(i64::MAX),
                Value::Float64(9_007_199_254_740_992.0),
                Value::Bool(false),
                Value::String("a-last".to_owned()),
            ])
            .expect("second row in appended group");

        assert_eq!(table.row_group_count(), 2);
        assert!(matches!(
            table.zone_map(0, 0),
            ZoneMapRef::Int64 { min: -10, max } if max == ROW_GROUP_SIZE as i64 - 11
        ));
        assert!(matches!(
            table.zone_map(0, 1),
            ZoneMapRef::Int64 {
                min: i64::MIN,
                max: i64::MAX
            }
        ));
        assert!(matches!(
            table.zone_map(1, 1),
            ZoneMapRef::Float64 { min: -1.5, max } if max == 9_007_199_254_740_992.0
        ));
        assert!(matches!(
            table.zone_map(2, 0),
            ZoneMapRef::Bool {
                has_false: true,
                has_true: false
            }
        ));
        assert!(matches!(
            table.zone_map(2, 1),
            ZoneMapRef::Bool {
                has_false: true,
                has_true: true
            }
        ));
        assert!(matches!(
            table.zone_map(3, 1),
            ZoneMapRef::String {
                min: "a-last",
                max: "z-last"
            }
        ));
    }
}
