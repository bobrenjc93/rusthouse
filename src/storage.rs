//! Typed, bounded, in-memory columnar storage.

use std::error::Error;
use std::fmt;

/// A scalar type supported by the storage layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataType {
    Int64,
    Float64,
    Bool,
    String,
}

impl fmt::Display for DataType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Int64 => "Int64",
            Self::Float64 => "Float64",
            Self::Bool => "Bool",
            Self::String => "String",
        };
        formatter.write_str(name)
    }
}

/// The name and type of one column in a [`Schema`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnSchema {
    name: String,
    data_type: DataType,
}

impl ColumnSchema {
    pub fn new(name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            name: name.into(),
            data_type,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn data_type(&self) -> DataType {
        self.data_type
    }
}

/// An ordered collection of column definitions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Schema {
    columns: Vec<ColumnSchema>,
}

impl Schema {
    pub fn new(columns: Vec<ColumnSchema>) -> Self {
        Self { columns }
    }

    pub fn columns(&self) -> &[ColumnSchema] {
        &self.columns
    }

    pub fn len(&self) -> usize {
        self.columns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }
}

/// One owned scalar value supplied to an insert.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Int64(i64),
    Float64(f64),
    Bool(bool),
    String(String),
}

impl Value {
    pub fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }
}

/// A comparison supported by a checked predicate scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComparisonOperator {
    Equal,
    NotEqual,
    LessThan,
    LessThanOrEqual,
    GreaterThan,
    GreaterThanOrEqual,
}

impl ComparisonOperator {
    fn is_ordered(self) -> bool {
        matches!(
            self,
            Self::LessThan | Self::LessThanOrEqual | Self::GreaterThan | Self::GreaterThanOrEqual
        )
    }
}

impl fmt::Display for ComparisonOperator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let operator = match self {
            Self::Equal => "=",
            Self::NotEqual => "!=",
            Self::LessThan => "<",
            Self::LessThanOrEqual => "<=",
            Self::GreaterThan => ">",
            Self::GreaterThanOrEqual => ">=",
        };
        formatter.write_str(operator)
    }
}

/// One logical row in a batch insert.
pub type Row = Vec<Value>;

/// A physical, homogeneous column vector.
#[derive(Clone, Debug, PartialEq)]
pub enum Column {
    Int64(Vec<i64>),
    Float64(Vec<f64>),
    Bool(Vec<bool>),
    String(Vec<String>),
}

impl Column {
    fn new(data_type: DataType) -> Self {
        match data_type {
            DataType::Int64 => Self::Int64(Vec::new()),
            DataType::Float64 => Self::Float64(Vec::new()),
            DataType::Bool => Self::Bool(Vec::new()),
            DataType::String => Self::String(Vec::new()),
        }
    }

    pub fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Int64(values) => values.len(),
            Self::Float64(values) => values.len(),
            Self::Bool(values) => values.len(),
            Self::String(values) => values.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn push(&mut self, value: Value) {
        match (self, value) {
            (Self::Int64(values), Value::Int64(value)) => values.push(value),
            (Self::Float64(values), Value::Float64(value)) => values.push(value),
            (Self::Bool(values), Value::Bool(value)) => values.push(value),
            (Self::String(values), Value::String(value)) => values.push(value),
            _ => unreachable!("batch values are type-checked before insertion"),
        }
    }
}

/// A validation failure that leaves the table's logical contents unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StorageError {
    CapacityExceeded {
        capacity: usize,
        current_rows: usize,
        batch_rows: usize,
    },
    RowWidth {
        row: usize,
        expected: usize,
        actual: usize,
    },
    TypeMismatch {
        row: usize,
        column: usize,
        expected: DataType,
        actual: DataType,
    },
    NonFiniteFloat {
        row: usize,
        column: usize,
    },
}

impl fmt::Display for StorageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapacityExceeded {
                capacity,
                current_rows,
                batch_rows,
            } => write!(
                formatter,
                "inserting {batch_rows} rows into a table with {current_rows} rows exceeds its capacity of {capacity}"
            ),
            Self::RowWidth {
                row,
                expected,
                actual,
            } => write!(
                formatter,
                "row {row} has {actual} values, but the schema requires {expected}"
            ),
            Self::TypeMismatch {
                row,
                column,
                expected,
                actual,
            } => write!(
                formatter,
                "row {row}, column {column} has type {actual}, but the schema requires {expected}"
            ),
            Self::NonFiniteFloat { row, column } => {
                write!(
                    formatter,
                    "row {row}, column {column} is not a finite Float64"
                )
            }
        }
    }
}

impl Error for StorageError {}

/// A validation failure produced before or during a predicate scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScanError {
    ColumnOutOfBounds {
        column: usize,
        column_count: usize,
    },
    TypeMismatch {
        column: usize,
        expected: DataType,
        actual: DataType,
    },
    UnsupportedOperator {
        column: usize,
        data_type: DataType,
        operator: ComparisonOperator,
    },
    NonFiniteFloat {
        column: usize,
    },
}

impl fmt::Display for ScanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ColumnOutOfBounds {
                column,
                column_count,
            } => write!(
                formatter,
                "column {column} is out of bounds for a table with {column_count} columns"
            ),
            Self::TypeMismatch {
                column,
                expected,
                actual,
            } => write!(
                formatter,
                "literal for column {column} has type {actual}, but the column requires {expected}"
            ),
            Self::UnsupportedOperator {
                column,
                data_type,
                operator,
            } => write!(
                formatter,
                "operator {operator} is not supported for {data_type} column {column}"
            ),
            Self::NonFiniteFloat { column } => {
                write!(
                    formatter,
                    "literal for column {column} is not a finite Float64"
                )
            }
        }
    }
}

impl Error for ScanError {}

/// A row-bounded table backed by one physical vector per schema column.
#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    schema: Schema,
    columns: Vec<Column>,
    row_count: usize,
    capacity: usize,
}

impl Table {
    pub fn new(schema: Schema, capacity: usize) -> Self {
        let columns = schema
            .columns()
            .iter()
            .map(|column| Column::new(column.data_type()))
            .collect();
        Self {
            schema,
            columns,
            row_count: 0,
            capacity,
        }
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    pub fn len(&self) -> usize {
        self.row_count
    }

    pub fn is_empty(&self) -> bool {
        self.row_count == 0
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns the row indices whose value in `column` matches `literal`.
    ///
    /// Equality and inequality are supported for every stored type. Ordered
    /// comparisons are supported for integer, finite float, and string
    /// columns. Matches retain their ascending physical row order.
    ///
    /// # Examples
    ///
    /// ```
    /// use rusthouse::{
    ///     ColumnSchema, ComparisonOperator, DataType, Schema, Table, Value,
    /// };
    ///
    /// let mut table = Table::new(
    ///     Schema::new(vec![ColumnSchema::new("amount", DataType::Int64)]),
    ///     2,
    /// );
    /// table
    ///     .insert_batch(vec![vec![Value::Int64(4)], vec![Value::Int64(9)]])
    ///     .unwrap();
    ///
    /// let matches = table
    ///     .scan(0, ComparisonOperator::GreaterThan, &Value::Int64(4))
    ///     .unwrap();
    /// assert_eq!(matches, vec![1]);
    /// ```
    pub fn scan(
        &self,
        column: usize,
        operator: ComparisonOperator,
        literal: &Value,
    ) -> Result<Vec<usize>, ScanError> {
        let stored_column = self
            .columns
            .get(column)
            .ok_or(ScanError::ColumnOutOfBounds {
                column,
                column_count: self.columns.len(),
            })?;
        let expected = stored_column.data_type();
        let actual = literal.data_type();
        if actual != expected {
            return Err(ScanError::TypeMismatch {
                column,
                expected,
                actual,
            });
        }
        if expected == DataType::Bool && operator.is_ordered() {
            return Err(ScanError::UnsupportedOperator {
                column,
                data_type: expected,
                operator,
            });
        }
        if matches!(literal, Value::Float64(value) if !value.is_finite()) {
            return Err(ScanError::NonFiniteFloat { column });
        }

        let matches = match (stored_column, literal) {
            (Column::Int64(values), Value::Int64(literal)) => {
                matching_indices(values, operator, literal)
            }
            (Column::Float64(values), Value::Float64(literal)) => {
                matching_indices(values, operator, literal)
            }
            (Column::Bool(values), Value::Bool(literal)) => {
                matching_indices(values, operator, literal)
            }
            (Column::String(values), Value::String(literal)) => {
                matching_indices(values, operator, literal)
            }
            _ => unreachable!("the scan literal type was validated against the column"),
        };
        Ok(matches)
    }

    /// Validates and atomically appends a batch of logical rows.
    ///
    /// Capacity, row widths, value types, and float finiteness are checked for
    /// every row before any physical column is changed.
    pub fn insert_batch(&mut self, rows: Vec<Row>) -> Result<(), StorageError> {
        self.validate_batch(&rows)?;

        let inserted_rows = rows.len();
        for row in rows {
            for (column, value) in self.columns.iter_mut().zip(row) {
                column.push(value);
            }
        }
        self.row_count += inserted_rows;
        Ok(())
    }

    fn validate_batch(&self, rows: &[Row]) -> Result<(), StorageError> {
        let fits = self
            .row_count
            .checked_add(rows.len())
            .is_some_and(|new_len| new_len <= self.capacity);
        if !fits {
            return Err(StorageError::CapacityExceeded {
                capacity: self.capacity,
                current_rows: self.row_count,
                batch_rows: rows.len(),
            });
        }

        for (row_index, row) in rows.iter().enumerate() {
            if row.len() != self.schema.len() {
                return Err(StorageError::RowWidth {
                    row: row_index,
                    expected: self.schema.len(),
                    actual: row.len(),
                });
            }

            for (column_index, (value, column)) in row.iter().zip(self.schema.columns()).enumerate()
            {
                let actual = value.data_type();
                let expected = column.data_type();
                if actual != expected {
                    return Err(StorageError::TypeMismatch {
                        row: row_index,
                        column: column_index,
                        expected,
                        actual,
                    });
                }
                if let Value::Float64(value) = value {
                    if !value.is_finite() {
                        return Err(StorageError::NonFiniteFloat {
                            row: row_index,
                            column: column_index,
                        });
                    }
                }
            }
        }
        Ok(())
    }
}

fn matching_indices<T: PartialOrd>(
    values: &[T],
    operator: ComparisonOperator,
    literal: &T,
) -> Vec<usize> {
    values
        .iter()
        .enumerate()
        .filter_map(|(index, value)| comparison_matches(value, operator, literal).then_some(index))
        .collect()
}

fn comparison_matches<T: PartialOrd>(value: &T, operator: ComparisonOperator, literal: &T) -> bool {
    match operator {
        ComparisonOperator::Equal => value == literal,
        ComparisonOperator::NotEqual => value != literal,
        ComparisonOperator::LessThan => value < literal,
        ComparisonOperator::LessThanOrEqual => value <= literal,
        ComparisonOperator::GreaterThan => value > literal,
        ComparisonOperator::GreaterThanOrEqual => value >= literal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn four_type_schema() -> Schema {
        Schema::new(vec![
            ColumnSchema::new("signed", DataType::Int64),
            ColumnSchema::new("ratio", DataType::Float64),
            ColumnSchema::new("enabled", DataType::Bool),
            ColumnSchema::new("label", DataType::String),
        ])
    }

    fn row(integer: i64, float: f64, boolean: bool, string: &str) -> Row {
        vec![
            Value::Int64(integer),
            Value::Float64(float),
            Value::Bool(boolean),
            Value::String(string.to_owned()),
        ]
    }

    fn populated_four_type_table() -> Table {
        let mut table = Table::new(four_type_schema(), 4);
        table
            .insert_batch(vec![
                row(-1, -1.5, false, "ant"),
                row(0, 0.0, true, "bee"),
                row(1, 1.5, true, "bee"),
                row(2, 2.0, false, "cat"),
            ])
            .unwrap();
        table
    }

    fn assert_scan(
        table: &Table,
        column: usize,
        operator: ComparisonOperator,
        literal: Value,
        expected: &[usize],
    ) {
        assert_eq!(table.scan(column, operator, &literal).unwrap(), expected);
    }

    #[test]
    fn stores_all_supported_types_in_physical_columns() {
        let mut table = Table::new(four_type_schema(), 3);

        table
            .insert_batch(vec![
                row(-7, 1.25, true, "first"),
                row(9, -2.5, false, "second"),
            ])
            .unwrap();

        assert_eq!(table.len(), 2);
        assert_eq!(table.capacity(), 3);
        assert_eq!(table.schema().columns()[0].name(), "signed");
        assert_eq!(
            table.columns(),
            &[
                Column::Int64(vec![-7, 9]),
                Column::Float64(vec![1.25, -2.5]),
                Column::Bool(vec![true, false]),
                Column::String(vec!["first".to_owned(), "second".to_owned()]),
            ]
        );
    }

    #[test]
    fn invalid_middle_row_rolls_back_the_whole_batch() {
        let mut table = Table::new(four_type_schema(), 5);
        table
            .insert_batch(vec![row(1, 1.0, true, "existing")])
            .unwrap();
        let before = table.clone();

        let error = table
            .insert_batch(vec![
                row(2, 2.0, false, "valid"),
                vec![
                    Value::Int64(3),
                    Value::Float64(3.0),
                    Value::String("not a bool".to_owned()),
                    Value::String("invalid".to_owned()),
                ],
                row(4, 4.0, true, "never inserted"),
            ])
            .unwrap_err();

        assert_eq!(
            error,
            StorageError::TypeMismatch {
                row: 1,
                column: 2,
                expected: DataType::Bool,
                actual: DataType::String,
            }
        );
        assert_eq!(table, before);
    }

    #[test]
    fn rejects_bad_row_width_without_mutation() {
        let mut table = Table::new(four_type_schema(), 2);

        let error = table.insert_batch(vec![vec![Value::Int64(1)]]).unwrap_err();

        assert_eq!(
            error,
            StorageError::RowWidth {
                row: 0,
                expected: 4,
                actual: 1,
            }
        );
        assert!(table.is_empty());
        assert!(table.columns().iter().all(Column::is_empty));
    }

    #[test]
    fn rejects_non_finite_floats_without_mutation() {
        for non_finite in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut table = Table::new(four_type_schema(), 1);

            let error = table
                .insert_batch(vec![row(1, non_finite, true, "invalid")])
                .unwrap_err();

            assert_eq!(StorageError::NonFiniteFloat { row: 0, column: 1 }, error);
            assert!(table.is_empty());
        }
    }

    #[test]
    fn rejects_batch_that_exceeds_remaining_capacity() {
        let mut table = Table::new(four_type_schema(), 2);
        table
            .insert_batch(vec![row(1, 1.0, true, "existing")])
            .unwrap();
        let before = table.clone();

        let error = table
            .insert_batch(vec![row(2, 2.0, false, "two"), row(3, 3.0, true, "three")])
            .unwrap_err();

        assert_eq!(
            error,
            StorageError::CapacityExceeded {
                capacity: 2,
                current_rows: 1,
                batch_rows: 2,
            }
        );
        assert_eq!(table, before);
    }

    #[test]
    fn reports_mismatches_for_each_supported_type() {
        let expected_types = [
            DataType::Int64,
            DataType::Float64,
            DataType::Bool,
            DataType::String,
        ];
        let wrong_values = [
            Value::Float64(1.0),
            Value::Bool(true),
            Value::String("wrong".to_owned()),
            Value::Int64(1),
        ];

        for (column, wrong_value) in wrong_values.into_iter().enumerate() {
            let mut values = row(1, 1.0, true, "valid");
            let actual = wrong_value.data_type();
            values[column] = wrong_value;
            let mut table = Table::new(four_type_schema(), 1);

            assert_eq!(
                table.insert_batch(vec![values]),
                Err(StorageError::TypeMismatch {
                    row: 0,
                    column,
                    expected: expected_types[column],
                    actual,
                })
            );
            assert!(table.is_empty());
        }
    }

    #[test]
    fn empty_batch_is_a_no_op_at_capacity() {
        let mut table = Table::new(Schema::default(), 0);

        table.insert_batch(Vec::new()).unwrap();

        assert!(table.is_empty());
        assert!(table.columns().is_empty());
    }

    #[test]
    fn scans_equality_and_inequality_for_every_stored_type() {
        let table = populated_four_type_table();
        let before = table.clone();

        assert_scan(&table, 0, ComparisonOperator::Equal, Value::Int64(0), &[1]);
        assert_scan(
            &table,
            1,
            ComparisonOperator::Equal,
            Value::Float64(1.5),
            &[2],
        );
        assert_scan(
            &table,
            2,
            ComparisonOperator::Equal,
            Value::Bool(true),
            &[1, 2],
        );
        assert_scan(
            &table,
            3,
            ComparisonOperator::Equal,
            Value::String("bee".to_owned()),
            &[1, 2],
        );

        assert_scan(
            &table,
            0,
            ComparisonOperator::NotEqual,
            Value::Int64(0),
            &[0, 2, 3],
        );
        assert_scan(
            &table,
            1,
            ComparisonOperator::NotEqual,
            Value::Float64(1.5),
            &[0, 1, 3],
        );
        assert_scan(
            &table,
            2,
            ComparisonOperator::NotEqual,
            Value::Bool(true),
            &[0, 3],
        );
        assert_scan(
            &table,
            3,
            ComparisonOperator::NotEqual,
            Value::String("bee".to_owned()),
            &[0, 3],
        );

        assert_eq!(table, before, "scans must not mutate table contents");
    }

    #[test]
    fn distinguishes_strict_and_inclusive_ordering_boundaries() {
        let table = populated_four_type_table();
        let ordered_columns = [
            (
                0,
                Value::Int64(1),
                vec![0, 1],
                vec![0, 1, 2],
                vec![3],
                vec![2, 3],
            ),
            (
                1,
                Value::Float64(1.5),
                vec![0, 1],
                vec![0, 1, 2],
                vec![3],
                vec![2, 3],
            ),
            (
                3,
                Value::String("bee".to_owned()),
                vec![0],
                vec![0, 1, 2],
                vec![3],
                vec![1, 2, 3],
            ),
        ];

        for (column, literal, less, less_equal, greater, greater_equal) in ordered_columns {
            assert_scan(
                &table,
                column,
                ComparisonOperator::LessThan,
                literal.clone(),
                &less,
            );
            assert_scan(
                &table,
                column,
                ComparisonOperator::LessThanOrEqual,
                literal.clone(),
                &less_equal,
            );
            assert_scan(
                &table,
                column,
                ComparisonOperator::GreaterThan,
                literal.clone(),
                &greater,
            );
            assert_scan(
                &table,
                column,
                ComparisonOperator::GreaterThanOrEqual,
                literal,
                &greater_equal,
            );
        }
    }

    #[test]
    fn returns_empty_selection_vectors_when_nothing_matches() {
        let table = populated_four_type_table();
        assert_scan(&table, 0, ComparisonOperator::Equal, Value::Int64(99), &[]);

        let empty = Table::new(four_type_schema(), 0);
        assert_scan(
            &empty,
            3,
            ComparisonOperator::LessThan,
            Value::String("anything".to_owned()),
            &[],
        );
    }

    #[test]
    fn rejects_out_of_bounds_scan_columns() {
        let table = populated_four_type_table();

        assert_eq!(
            table.scan(4, ComparisonOperator::Equal, &Value::Int64(0)),
            Err(ScanError::ColumnOutOfBounds {
                column: 4,
                column_count: 4,
            })
        );
    }

    #[test]
    fn rejects_scan_literals_with_the_wrong_type() {
        let table = populated_four_type_table();
        let cases = [
            (0, DataType::Int64, Value::String("0".to_owned())),
            (1, DataType::Float64, Value::Bool(false)),
            (2, DataType::Bool, Value::Int64(0)),
            (3, DataType::String, Value::Float64(0.0)),
        ];

        for (column, expected, literal) in cases {
            let actual = literal.data_type();
            assert_eq!(
                table.scan(column, ComparisonOperator::Equal, &literal),
                Err(ScanError::TypeMismatch {
                    column,
                    expected,
                    actual,
                })
            );
        }
    }

    #[test]
    fn rejects_ordered_comparisons_for_boolean_columns() {
        let table = populated_four_type_table();
        let ordered_operators = [
            ComparisonOperator::LessThan,
            ComparisonOperator::LessThanOrEqual,
            ComparisonOperator::GreaterThan,
            ComparisonOperator::GreaterThanOrEqual,
        ];

        for operator in ordered_operators {
            assert_eq!(
                table.scan(2, operator, &Value::Bool(false)),
                Err(ScanError::UnsupportedOperator {
                    column: 2,
                    data_type: DataType::Bool,
                    operator,
                })
            );
        }
    }

    #[test]
    fn rejects_non_finite_float_scan_literals() {
        let table = populated_four_type_table();

        for literal in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                table.scan(1, ComparisonOperator::Equal, &Value::Float64(literal)),
                Err(ScanError::NonFiniteFloat { column: 1 })
            );
        }
    }
}
