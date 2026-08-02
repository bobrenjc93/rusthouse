use std::collections::HashSet;
use std::error::Error;
use std::fmt;

/// A physical type supported by RustHouse's in-memory storage.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DataType {
    /// A signed 64-bit integer.
    Int64,
    /// A finite IEEE 754 double-precision number.
    Float64,
    /// A boolean value.
    Bool,
    /// A UTF-8 string.
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

/// A typed value accepted by [`Table::insert_batch`].
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// A signed 64-bit integer.
    Int64(i64),
    /// An IEEE 754 double-precision number.
    Float64(f64),
    /// A boolean value.
    Bool(bool),
    /// A UTF-8 string.
    String(String),
}

impl Value {
    /// Returns the physical type of this value.
    #[must_use]
    pub const fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }
}

impl From<i64> for Value {
    fn from(value: i64) -> Self {
        Self::Int64(value)
    }
}

impl From<f64> for Value {
    fn from(value: f64) -> Self {
        Self::Float64(value)
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}

/// The name and physical type of one table column.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ColumnDef {
    /// The unique column name.
    pub name: String,
    /// The values stored in the column.
    pub data_type: DataType,
}

impl ColumnDef {
    /// Creates a column definition. Names are validated when a [`Table`] is created.
    pub fn new(name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            name: name.into(),
            data_type,
        }
    }
}

/// A schema or batch validation failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TableError {
    /// A table must have at least one column.
    EmptySchema,
    /// A column name must contain a non-whitespace character.
    EmptyColumnName {
        /// The zero-based position of the invalid definition.
        column: usize,
    },
    /// Column names must be unique within a table.
    DuplicateColumn {
        /// The repeated name.
        name: String,
    },
    /// Every input row must contain exactly one value per column.
    WrongRowWidth {
        /// The zero-based position of the invalid row in the input batch.
        row: usize,
        /// The number of columns in the table.
        expected: usize,
        /// The number of values supplied by the row.
        actual: usize,
    },
    /// An input value did not match its column's physical type.
    TypeMismatch {
        /// The zero-based position of the value's row in the input batch.
        row: usize,
        /// The zero-based position of the value's column.
        column: usize,
        /// The column's physical type.
        expected: DataType,
        /// The input value's physical type.
        actual: DataType,
    },
    /// Float columns reject NaN and positive or negative infinity.
    NonFiniteFloat {
        /// The zero-based position of the value's row in the input batch.
        row: usize,
        /// The zero-based position of the value's column.
        column: usize,
    },
}

impl fmt::Display for TableError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySchema => formatter.write_str("a table schema cannot be empty"),
            Self::EmptyColumnName { column } => {
                write!(formatter, "column {column} has an empty name")
            }
            Self::DuplicateColumn { name } => {
                write!(formatter, "column name {name:?} appears more than once")
            }
            Self::WrongRowWidth {
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

impl Error for TableError {}

#[derive(Clone, Debug, PartialEq)]
enum Column {
    Int64(Vec<i64>),
    Float64(Vec<f64>),
    Bool(Vec<bool>),
    String(Vec<String>),
}

impl Column {
    fn empty(data_type: DataType) -> Self {
        match data_type {
            DataType::Int64 => Self::Int64(Vec::new()),
            DataType::Float64 => Self::Float64(Vec::new()),
            DataType::Bool => Self::Bool(Vec::new()),
            DataType::String => Self::String(Vec::new()),
        }
    }

    fn push(&mut self, value: Value, row: usize, column: usize) -> Result<(), TableError> {
        let actual = value.data_type();
        match (self, value) {
            (Self::Int64(values), Value::Int64(value)) => values.push(value),
            (Self::Float64(values), Value::Float64(value)) if value.is_finite() => {
                values.push(value);
            }
            (Self::Float64(_), Value::Float64(_)) => {
                return Err(TableError::NonFiniteFloat { row, column });
            }
            (Self::Bool(values), Value::Bool(value)) => values.push(value),
            (Self::String(values), Value::String(value)) => values.push(value),
            (column_values, _) => {
                return Err(TableError::TypeMismatch {
                    row,
                    column,
                    expected: column_values.data_type(),
                    actual,
                });
            }
        }
        Ok(())
    }

    const fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }

    fn append(&mut self, other: &mut Self) {
        match (self, other) {
            (Self::Int64(values), Self::Int64(batch)) => values.append(batch),
            (Self::Float64(values), Self::Float64(batch)) => values.append(batch),
            (Self::Bool(values), Self::Bool(batch)) => values.append(batch),
            (Self::String(values), Self::String(batch)) => values.append(batch),
            _ => unreachable!("a staged column always has the table schema's type"),
        }
    }
}

/// An in-memory batch stored as one typed vector per schema column.
///
/// All rows passed to [`Table::insert_batch`] are validated before the table is
/// changed. If any row is invalid, the entire batch is rejected.
#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    schema: Vec<ColumnDef>,
    columns: Vec<Column>,
    row_count: usize,
}

impl Table {
    /// Creates an empty table after validating its schema.
    pub fn new(schema: Vec<ColumnDef>) -> Result<Self, TableError> {
        validate_schema(&schema)?;
        let columns = schema
            .iter()
            .map(|definition| Column::empty(definition.data_type))
            .collect();

        Ok(Self {
            schema,
            columns,
            row_count: 0,
        })
    }

    /// Returns the table's ordered schema.
    #[must_use]
    pub fn schema(&self) -> &[ColumnDef] {
        &self.schema
    }

    /// Returns the number of rows in the table.
    #[must_use]
    pub const fn row_count(&self) -> usize {
        self.row_count
    }

    /// Returns whether the table contains no rows.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.row_count == 0
    }

    /// Returns a column's position by its exact, case-sensitive name.
    #[must_use]
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.schema
            .iter()
            .position(|definition| definition.name == name)
    }

    /// Returns an `Int64` column, or `None` for another type or invalid index.
    #[must_use]
    pub fn int64_column(&self, index: usize) -> Option<&[i64]> {
        match self.columns.get(index)? {
            Column::Int64(values) => Some(values),
            _ => None,
        }
    }

    /// Returns a `Float64` column, or `None` for another type or invalid index.
    #[must_use]
    pub fn float64_column(&self, index: usize) -> Option<&[f64]> {
        match self.columns.get(index)? {
            Column::Float64(values) => Some(values),
            _ => None,
        }
    }

    /// Returns a `Bool` column, or `None` for another type or invalid index.
    #[must_use]
    pub fn bool_column(&self, index: usize) -> Option<&[bool]> {
        match self.columns.get(index)? {
            Column::Bool(values) => Some(values),
            _ => None,
        }
    }

    /// Returns a `String` column, or `None` for another type or invalid index.
    #[must_use]
    pub fn string_column(&self, index: usize) -> Option<&[String]> {
        match self.columns.get(index)? {
            Column::String(values) => Some(values),
            _ => None,
        }
    }

    /// Validates and atomically appends a whole row batch.
    ///
    /// An empty batch is a successful no-op. Error row positions are relative
    /// to the input batch, not to rows already stored in the table.
    pub fn insert_batch(&mut self, rows: Vec<Vec<Value>>) -> Result<(), TableError> {
        let batch_row_count = rows.len();
        let mut staged: Vec<Column> = self
            .schema
            .iter()
            .map(|definition| Column::empty(definition.data_type))
            .collect();

        for (row_index, row) in rows.into_iter().enumerate() {
            if row.len() != self.columns.len() {
                return Err(TableError::WrongRowWidth {
                    row: row_index,
                    expected: self.columns.len(),
                    actual: row.len(),
                });
            }

            for (column_index, (column, value)) in staged.iter_mut().zip(row).enumerate() {
                column.push(value, row_index, column_index)?;
            }
        }

        for (column, batch) in self.columns.iter_mut().zip(&mut staged) {
            column.append(batch);
        }
        self.row_count += batch_row_count;
        Ok(())
    }
}

fn validate_schema(schema: &[ColumnDef]) -> Result<(), TableError> {
    if schema.is_empty() {
        return Err(TableError::EmptySchema);
    }

    let mut names = HashSet::with_capacity(schema.len());
    for (column, definition) in schema.iter().enumerate() {
        if definition.name.trim().is_empty() {
            return Err(TableError::EmptyColumnName { column });
        }
        if !names.insert(definition.name.as_str()) {
            return Err(TableError::DuplicateColumn {
                name: definition.name.clone(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_types_schema() -> Vec<ColumnDef> {
        vec![
            ColumnDef::new("id", DataType::Int64),
            ColumnDef::new("score", DataType::Float64),
            ColumnDef::new("active", DataType::Bool),
            ColumnDef::new("label", DataType::String),
        ]
    }

    fn row(id: i64, score: f64, active: bool, label: &str) -> Vec<Value> {
        vec![id.into(), score.into(), active.into(), label.into()]
    }

    #[test]
    fn stores_each_type_in_its_own_column() {
        let mut table = Table::new(all_types_schema()).unwrap();

        table
            .insert_batch(vec![row(1, 1.25, true, "one"), row(-2, 3.5, false, "two")])
            .unwrap();

        assert_eq!(table.row_count(), 2);
        assert!(!table.is_empty());
        assert_eq!(table.column_index("score"), Some(1));
        assert_eq!(table.int64_column(0), Some([1, -2].as_slice()));
        assert_eq!(table.float64_column(1), Some([1.25, 3.5].as_slice()));
        assert_eq!(table.bool_column(2), Some([true, false].as_slice()));
        assert_eq!(
            table.string_column(3),
            Some([String::from("one"), String::from("two")].as_slice())
        );
        assert_eq!(table.int64_column(1), None);
    }

    #[test]
    fn rejects_empty_and_invalid_schemas() {
        assert_eq!(Table::new(vec![]), Err(TableError::EmptySchema));
        assert_eq!(
            Table::new(vec![ColumnDef::new("  ", DataType::Bool)]),
            Err(TableError::EmptyColumnName { column: 0 })
        );
        assert_eq!(
            Table::new(vec![
                ColumnDef::new("id", DataType::Int64),
                ColumnDef::new("id", DataType::String),
            ]),
            Err(TableError::DuplicateColumn {
                name: String::from("id")
            })
        );
    }

    #[test]
    fn wrong_width_rolls_back_the_whole_batch() {
        let mut table = Table::new(all_types_schema()).unwrap();
        table
            .insert_batch(vec![row(10, 2.0, true, "existing")])
            .unwrap();
        let before = table.clone();

        let error = table
            .insert_batch(vec![row(20, 4.0, false, "valid"), vec![21.into()]])
            .unwrap_err();

        assert_eq!(
            error,
            TableError::WrongRowWidth {
                row: 1,
                expected: 4,
                actual: 1,
            }
        );
        assert_eq!(table, before);
    }

    #[test]
    fn type_mismatch_rolls_back_the_whole_batch() {
        let mut table = Table::new(all_types_schema()).unwrap();
        table
            .insert_batch(vec![row(10, 2.0, true, "existing")])
            .unwrap();
        let before = table.clone();

        let error = table
            .insert_batch(vec![
                row(20, 4.0, false, "valid"),
                vec![21.into(), true.into(), false.into(), "invalid".into()],
            ])
            .unwrap_err();

        assert_eq!(
            error,
            TableError::TypeMismatch {
                row: 1,
                column: 1,
                expected: DataType::Float64,
                actual: DataType::Bool,
            }
        );
        assert_eq!(table, before);
    }

    #[test]
    fn every_non_finite_float_rolls_back_the_whole_batch() {
        for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut table = Table::new(all_types_schema()).unwrap();
            table
                .insert_batch(vec![row(10, 2.0, true, "existing")])
                .unwrap();
            let before = table.clone();

            let error = table
                .insert_batch(vec![
                    row(20, 4.0, false, "valid"),
                    row(21, invalid, true, "invalid"),
                ])
                .unwrap_err();

            assert_eq!(TableError::NonFiniteFloat { row: 1, column: 1 }, error);
            assert_eq!(table, before);
        }
    }

    #[test]
    fn empty_batch_is_a_no_op() {
        let mut table = Table::new(all_types_schema()).unwrap();
        let before = table.clone();

        table.insert_batch(vec![]).unwrap();

        assert_eq!(table, before);
    }
}
