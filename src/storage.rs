use std::collections::HashSet;

use crate::error::{Error, Result};
use crate::value::{DataType, Value, ValueRef};

pub const DEFAULT_BLOOM_FALSE_POSITIVE_RATE: f64 = 0.025;
pub const DEFAULT_BLOOM_GRANULE_ROWS: usize = 64;
const MAX_BLOOM_BITS_PER_GRANULE: usize = 16 * 1024 * 1024;

/// Definition of a Bloom-filter data-skipping index over one column.
#[derive(Debug, Clone, PartialEq)]
pub struct BloomIndexDef {
    pub name: String,
    pub column: String,
    pub false_positive_rate: f64,
    pub granule_rows: usize,
}

impl BloomIndexDef {
    #[must_use]
    pub fn new(name: String, column: String) -> Self {
        Self {
            name,
            column,
            false_positive_rate: DEFAULT_BLOOM_FALSE_POSITIVE_RATE,
            granule_rows: DEFAULT_BLOOM_GRANULE_ROWS,
        }
    }
}

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
    UInt64(Vec<u64>),
    Float64(Vec<f64>),
    Bool(Vec<bool>),
    String(Vec<String>),
}

impl Column {
    #[must_use]
    pub fn new(data_type: DataType) -> Self {
        match data_type {
            DataType::Int64 => Self::Int64(Vec::new()),
            DataType::UInt64 => Self::UInt64(Vec::new()),
            DataType::Float64 => Self::Float64(Vec::new()),
            DataType::Bool => Self::Bool(Vec::new()),
            DataType::String => Self::String(Vec::new()),
        }
    }

    #[must_use]
    pub fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::UInt64(_) => DataType::UInt64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Int64(values) => values.len(),
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
            Self::Int64(values) => ValueRef::Int64(values[row]),
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
            (Self::Int64(values), Value::Int64(value)) => values.push(value),
            (Self::UInt64(values), Value::UInt64(value)) => values.push(value),
            (Self::Float64(values), Value::Float64(value)) => values.push(value),
            (Self::Bool(values), Value::Bool(value)) => values.push(value),
            (Self::String(values), Value::String(value)) => values.push(value),
            _ => unreachable!("values are validated before insertion"),
        }
    }
}

/// Runtime metadata for a Bloom index. Filters are stored per fixed row granule.
#[derive(Debug, Clone)]
pub struct BloomIndex {
    name: String,
    column: usize,
    column_name: String,
    false_positive_rate: f64,
    granule_rows: usize,
    bit_count: usize,
    hash_functions: usize,
    granules: Vec<BloomFilter>,
}

impl BloomIndex {
    fn build(definition: BloomIndexDef, column: usize, values: &Column) -> Result<Self> {
        if !matches!(
            values,
            Column::Int64(_) | Column::UInt64(_) | Column::String(_)
        ) {
            return Err(Error::InvalidQuery(format!(
                "Bloom index '{}' requires an Int64, UInt64, or String column; '{}' is {}",
                definition.name,
                definition.column,
                values.data_type()
            )));
        }
        if definition.granule_rows == 0 {
            return Err(Error::InvalidQuery(format!(
                "Bloom index '{}' GRANULARITY must be greater than zero",
                definition.name
            )));
        }
        if !definition.false_positive_rate.is_finite()
            || !(0.0..1.0).contains(&definition.false_positive_rate)
            || definition.false_positive_rate == 0.0
        {
            return Err(Error::InvalidQuery(format!(
                "Bloom index '{}' false-positive rate must be between 0 and 1",
                definition.name
            )));
        }

        let expected_items = definition.granule_rows as f64;
        let bit_count = (-(expected_items * definition.false_positive_rate.ln())
            / std::f64::consts::LN_2.powi(2))
        .ceil();
        if !bit_count.is_finite() || bit_count > MAX_BLOOM_BITS_PER_GRANULE as f64 {
            return Err(Error::InvalidQuery(format!(
                "Bloom index '{}' configuration requires too much memory per granule",
                definition.name
            )));
        }
        let bit_count = (bit_count as usize).max(1);
        let hash_functions = ((bit_count as f64 / expected_items) * std::f64::consts::LN_2)
            .round()
            .clamp(1.0, 16.0) as usize;
        let mut index = Self {
            name: definition.name,
            column,
            column_name: definition.column,
            false_positive_rate: definition.false_positive_rate,
            granule_rows: definition.granule_rows,
            bit_count,
            hash_functions,
            granules: Vec::new(),
        };
        index.granules = index.build_filters(values);
        Ok(index)
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn column_name(&self) -> &str {
        &self.column_name
    }

    #[must_use]
    pub fn false_positive_rate(&self) -> f64 {
        self.false_positive_rate
    }

    #[must_use]
    pub fn granule_rows(&self) -> usize {
        self.granule_rows
    }

    #[must_use]
    pub fn granule_count(&self) -> usize {
        self.granules.len()
    }

    fn empty_filter(&self) -> BloomFilter {
        BloomFilter::new(self.bit_count, self.hash_functions)
    }

    fn build_filters(&self, values: &Column) -> Vec<BloomFilter> {
        let mut filters = Vec::with_capacity(values.len().div_ceil(self.granule_rows));
        for row in 0..values.len() {
            if row % self.granule_rows == 0 {
                filters.push(self.empty_filter());
            }
            filters
                .last_mut()
                .expect("a granule was created for the row")
                .insert(values.value_ref(row));
        }
        filters
    }

    fn append(&mut self, value: ValueRef<'_>, row: usize) {
        let granule = row / self.granule_rows;
        if granule == self.granules.len() {
            self.granules.push(self.empty_filter());
        }
        debug_assert_eq!(granule + 1, self.granules.len());
        self.granules[granule].insert(value);
    }

    fn might_contain(&self, value: ValueRef<'_>, row: usize) -> bool {
        self.granules[row / self.granule_rows].might_contain(value)
    }
}

#[derive(Debug, Clone)]
struct BloomFilter {
    bits: Vec<u64>,
    bit_count: usize,
    hash_functions: usize,
}

impl BloomFilter {
    fn new(bit_count: usize, hash_functions: usize) -> Self {
        Self {
            bits: vec![0; bit_count.div_ceil(64)],
            bit_count,
            hash_functions,
        }
    }

    fn insert(&mut self, value: ValueRef<'_>) {
        let (first, second) = bloom_hashes(value);
        for index in 0..self.hash_functions {
            let bit =
                first.wrapping_add((index as u64).wrapping_mul(second)) as usize % self.bit_count;
            self.bits[bit / 64] |= 1_u64 << (bit % 64);
        }
    }

    fn might_contain(&self, value: ValueRef<'_>) -> bool {
        let (first, second) = bloom_hashes(value);
        (0..self.hash_functions).all(|index| {
            let bit =
                first.wrapping_add((index as u64).wrapping_mul(second)) as usize % self.bit_count;
            self.bits[bit / 64] & (1_u64 << (bit % 64)) != 0
        })
    }
}

fn bloom_hashes(value: ValueRef<'_>) -> (u64, u64) {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut feed = |byte: u8| {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    };
    match value {
        ValueRef::Int64(value) => {
            feed(0);
            for byte in value.to_le_bytes() {
                feed(byte);
            }
        }
        ValueRef::UInt64(value) => {
            feed(1);
            for byte in value.to_le_bytes() {
                feed(byte);
            }
        }
        ValueRef::String(value) => {
            feed(2);
            for byte in value.as_bytes() {
                feed(*byte);
            }
        }
        ValueRef::Float64(_) | ValueRef::Bool(_) => {
            unreachable!("Bloom indexes only contain supported types")
        }
    }
    let first = mix_hash(hash);
    let second = mix_hash(hash ^ 0x9e37_79b9_7f4a_7c15) | 1;
    (first, second)
}

fn mix_hash(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

/// A table stores one typed vector per schema field.
#[derive(Debug, Clone)]
pub struct Table {
    name: String,
    schema: Vec<ColumnDef>,
    columns: Vec<Column>,
    row_count: usize,
    indexes: Vec<BloomIndex>,
}

impl Table {
    pub fn new(name: String, schema: Vec<ColumnDef>) -> Result<Self> {
        Self::new_with_indexes(name, schema, Vec::new())
    }

    pub fn new_with_indexes(
        name: String,
        schema: Vec<ColumnDef>,
        index_definitions: Vec<BloomIndexDef>,
    ) -> Result<Self> {
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
        let mut table = Self {
            name,
            schema,
            columns,
            row_count: 0,
            indexes: Vec::new(),
        };
        for definition in index_definitions {
            table.add_bloom_index(definition)?;
        }
        Ok(table)
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
    pub fn indexes(&self) -> &[BloomIndex] {
        &self.indexes
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
            let compatible_integer = matches!(
                (field.data_type, value),
                (DataType::UInt64, Value::Int64(value)) if *value >= 0
            ) || matches!(
                (field.data_type, value),
                (DataType::Int64, Value::UInt64(value)) if *value <= i64::MAX as u64
            );
            if field.data_type != value.data_type() && !compatible_integer {
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

    /// Validates and normalizes a complete insert before changing columns or indexes.
    pub fn insert_rows(&mut self, rows: Vec<Vec<Value>>) -> Result<()> {
        for row in &rows {
            self.validate_row(row)?;
        }
        for row in rows {
            let row = self.normalize_row(row);
            let inserted_row = self.row_count;
            for (column, value) in self.columns.iter_mut().zip(row) {
                column.push(value);
            }
            for index in &mut self.indexes {
                index.append(
                    self.columns[index.column].value_ref(inserted_row),
                    inserted_row,
                );
            }
            self.row_count += 1;
        }
        Ok(())
    }

    fn normalize_row(&self, row: Vec<Value>) -> Vec<Value> {
        self.schema
            .iter()
            .zip(row)
            .map(|(field, value)| match (field.data_type, value) {
                (DataType::UInt64, Value::Int64(value)) => Value::UInt64(value as u64),
                (DataType::Int64, Value::UInt64(value)) => Value::Int64(value as i64),
                (_, value) => value,
            })
            .collect()
    }

    pub fn add_bloom_index(&mut self, definition: BloomIndexDef) -> Result<()> {
        if self
            .indexes
            .iter()
            .any(|index| index.name.eq_ignore_ascii_case(&definition.name))
        {
            return Err(Error::InvalidQuery(format!(
                "index '{}' already exists on table '{}'",
                definition.name, self.name
            )));
        }
        let column = self.column_index(&definition.column)?;
        let index = BloomIndex::build(definition, column, &self.columns[column])?;
        self.indexes.push(index);
        Ok(())
    }

    /// Rebuilds all index filters from column storage before replacing live filters.
    pub fn rebuild_indexes(&mut self) {
        let rebuilt = self
            .indexes
            .iter()
            .map(|index| index.build_filters(&self.columns[index.column]))
            .collect::<Vec<_>>();
        for (index, filters) in self.indexes.iter_mut().zip(rebuilt) {
            index.granules = filters;
        }
    }

    pub fn rebuild_index(&mut self, name: &str) -> Result<()> {
        let position = self
            .indexes
            .iter()
            .position(|index| index.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| {
                Error::InvalidQuery(format!(
                    "index '{name}' does not exist on table '{}'",
                    self.name
                ))
            })?;
        let filters =
            self.indexes[position].build_filters(&self.columns[self.indexes[position].column]);
        self.indexes[position].granules = filters;
        Ok(())
    }

    pub(crate) fn next_index_boundary(&self, row: usize) -> usize {
        self.indexes
            .iter()
            .map(|index| {
                row.saturating_add(index.granule_rows - row % index.granule_rows)
                    .min(self.row_count)
            })
            .min()
            .unwrap_or(self.row_count)
    }

    pub(crate) fn bloom_might_contain(
        &self,
        column: usize,
        value: ValueRef<'_>,
        row: usize,
    ) -> Option<bool> {
        let value = normalize_bloom_value(self.columns[column].data_type(), value)?;
        let mut found = false;
        for index in self.indexes.iter().filter(|index| index.column == column) {
            found = true;
            if !index.might_contain(value, row) {
                return Some(false);
            }
        }
        found.then_some(true)
    }
}

fn normalize_bloom_value(data_type: DataType, value: ValueRef<'_>) -> Option<ValueRef<'_>> {
    match (data_type, value) {
        (DataType::Int64, ValueRef::Int64(value)) => Some(ValueRef::Int64(value)),
        (DataType::Int64, ValueRef::UInt64(value)) => {
            i64::try_from(value).ok().map(ValueRef::Int64)
        }
        (DataType::UInt64, ValueRef::UInt64(value)) => Some(ValueRef::UInt64(value)),
        (DataType::UInt64, ValueRef::Int64(value)) => {
            u64::try_from(value).ok().map(ValueRef::UInt64)
        }
        (DataType::String, ValueRef::String(value)) => Some(ValueRef::String(value)),
        _ => None,
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
    fn bloom_collisions_are_false_positives_not_false_negatives() {
        let mut filter = BloomFilter::new(1, 1);
        filter.insert(ValueRef::String("present"));

        assert!(filter.might_contain(ValueRef::String("present")));
        assert!(filter.might_contain(ValueRef::String("absent")));
    }

    #[test]
    fn failed_batch_does_not_change_columns_or_index_granules() {
        let mut table = Table::new_with_indexes(
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
            vec![BloomIndexDef {
                name: "id_bloom".to_owned(),
                column: "id".to_owned(),
                false_positive_rate: 0.01,
                granule_rows: 2,
            }],
        )
        .expect("valid table");
        table
            .insert_rows(vec![
                vec![Value::Int64(1), Value::String("one".to_owned())],
                vec![Value::Int64(2), Value::String("two".to_owned())],
            ])
            .expect("valid batch");

        let error = table
            .insert_rows(vec![
                vec![Value::Int64(3), Value::String("three".to_owned())],
                vec![Value::Int64(4), Value::Bool(false)],
            ])
            .expect_err("second row is invalid");

        assert!(matches!(error, Error::TypeMismatch { .. }));
        assert_eq!(table.row_count(), 2);
        assert_eq!(table.indexes()[0].granule_count(), 1);
        assert!(matches!(&table.columns()[0], Column::Int64(values) if values == &[1, 2]));
    }

    #[test]
    fn rebuild_replaces_filters_from_column_values() {
        let mut table = Table::new_with_indexes(
            "events".to_owned(),
            vec![ColumnDef {
                name: "id".to_owned(),
                data_type: DataType::Int64,
            }],
            vec![BloomIndexDef {
                name: "id_bloom".to_owned(),
                column: "id".to_owned(),
                false_positive_rate: 0.01,
                granule_rows: 4,
            }],
        )
        .expect("valid table");
        table.insert_row(vec![Value::Int64(7)]).expect("valid row");
        table.indexes[0].granules[0].bits.fill(0);
        assert_eq!(
            table.bloom_might_contain(0, ValueRef::Int64(7), 0),
            Some(false)
        );

        table.rebuild_indexes();

        assert_eq!(
            table.bloom_might_contain(0, ValueRef::Int64(7), 0),
            Some(true)
        );
    }
}
