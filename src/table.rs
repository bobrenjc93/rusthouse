use std::collections::HashSet;
use std::error::Error;
use std::fmt;

/// The scalar types supported by an in-memory table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataType {
    Int64,
    Float64,
    Bool,
    String,
}

impl fmt::Display for DataType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Int64 => "Int64",
            Self::Float64 => "Float64",
            Self::Bool => "Bool",
            Self::String => "String",
        })
    }
}

/// A single owned cell value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Int64(i64),
    Float64(f64),
    Bool(bool),
    String(String),
}

impl Value {
    /// Returns the type represented by this value.
    pub fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }
}

/// A named field in a table schema.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub data_type: DataType,
}

impl Field {
    pub fn new(name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            name: name.into(),
            data_type,
        }
    }
}

/// A validated, ordered collection of fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Schema {
    fields: Vec<Field>,
}

impl Schema {
    /// Creates a schema, rejecting repeated field names.
    pub fn new(fields: Vec<Field>) -> Result<Self, SchemaError> {
        let mut names = HashSet::with_capacity(fields.len());
        for field in &fields {
            if !names.insert(field.name.as_str()) {
                return Err(SchemaError::DuplicateField {
                    name: field.name.clone(),
                });
            }
        }

        Ok(Self { fields })
    }

    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

/// Failure to construct a valid schema.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SchemaError {
    DuplicateField { name: String },
}

impl fmt::Display for SchemaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateField { name } => write!(formatter, "duplicate field `{name}`"),
        }
    }
}

impl Error for SchemaError {}

/// A homogeneous, contiguous column of values.
#[derive(Clone, Debug, PartialEq)]
pub enum Column {
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
            _ => unreachable!("row values are checked before columns are updated"),
        }
    }
}

/// A schema-checked table backed by one homogeneous vector per field.
#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    schema: Schema,
    columns: Vec<Column>,
    row_count: usize,
}

impl Table {
    pub fn new(schema: Schema) -> Self {
        let columns = schema
            .fields()
            .iter()
            .map(|field| Column::empty(field.data_type))
            .collect();

        Self {
            schema,
            columns,
            row_count: 0,
        }
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    pub fn column(&self, index: usize) -> Option<&Column> {
        self.columns.get(index)
    }

    pub fn row_count(&self) -> usize {
        self.row_count
    }

    pub fn is_empty(&self) -> bool {
        self.row_count == 0
    }

    /// Appends a row only after its complete shape and types have been checked.
    pub fn append_row(&mut self, row: Vec<Value>) -> Result<(), TableError> {
        if row.len() != self.columns.len() {
            return Err(TableError::RowWidthMismatch {
                expected: self.columns.len(),
                actual: row.len(),
            });
        }

        for (index, (field, value)) in self.schema.fields().iter().zip(&row).enumerate() {
            let actual = value.data_type();
            if field.data_type != actual {
                return Err(TableError::TypeMismatch {
                    column: index,
                    field: field.name.clone(),
                    expected: field.data_type,
                    actual,
                });
            }
        }

        for (column, value) in self.columns.iter_mut().zip(row) {
            column.push(value);
        }
        self.row_count += 1;
        Ok(())
    }
}

/// Failure to append a row to a table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TableError {
    RowWidthMismatch {
        expected: usize,
        actual: usize,
    },
    TypeMismatch {
        column: usize,
        field: String,
        expected: DataType,
        actual: DataType,
    },
}

impl fmt::Display for TableError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RowWidthMismatch { expected, actual } => {
                write!(
                    formatter,
                    "row has {actual} values but schema requires {expected}"
                )
            }
            Self::TypeMismatch {
                column,
                field,
                expected,
                actual,
            } => write!(
                formatter,
                "column {column} (`{field}`) expects {expected} but received {actual}"
            ),
        }
    }
}

impl Error for TableError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_types_schema() -> Schema {
        Schema::new(vec![
            Field::new("integer", DataType::Int64),
            Field::new("float", DataType::Float64),
            Field::new("boolean", DataType::Bool),
            Field::new("string", DataType::String),
        ])
        .unwrap()
    }

    fn first_row() -> Vec<Value> {
        vec![
            Value::Int64(-42),
            Value::Float64(3.5),
            Value::Bool(true),
            Value::String("columnar".to_owned()),
        ]
    }

    #[test]
    fn stores_every_supported_type_in_a_homogeneous_column() {
        let schema = all_types_schema();
        let mut table = Table::new(schema.clone());

        table.append_row(first_row()).unwrap();
        table
            .append_row(vec![
                Value::Int64(7),
                Value::Float64(-0.25),
                Value::Bool(false),
                Value::String("vectors".to_owned()),
            ])
            .unwrap();

        assert_eq!(table.schema(), &schema);
        assert_eq!(table.row_count(), 2);
        assert_eq!(
            table.columns(),
            &[
                Column::Int64(vec![-42, 7]),
                Column::Float64(vec![3.5, -0.25]),
                Column::Bool(vec![true, false]),
                Column::String(vec!["columnar".to_owned(), "vectors".to_owned()]),
            ]
        );
        assert!(table.columns().iter().all(|column| column.len() == 2));
    }

    #[test]
    fn rejects_duplicate_field_names() {
        let error = Schema::new(vec![
            Field::new("event", DataType::Int64),
            Field::new("event", DataType::String),
        ])
        .unwrap_err();

        assert_eq!(
            error,
            SchemaError::DuplicateField {
                name: "event".to_owned()
            }
        );
    }

    #[test]
    fn width_mismatch_does_not_append_any_values() {
        let mut table = Table::new(all_types_schema());
        table.append_row(first_row()).unwrap();
        let before = table.clone();

        let error = table
            .append_row(vec![Value::Int64(1), Value::Float64(2.0)])
            .unwrap_err();

        assert_eq!(
            error,
            TableError::RowWidthMismatch {
                expected: 4,
                actual: 2
            }
        );
        assert_eq!(table, before);
    }

    #[test]
    fn type_mismatch_does_not_partially_append_a_row() {
        let mut table = Table::new(all_types_schema());
        table.append_row(first_row()).unwrap();
        let before = table.clone();

        let error = table
            .append_row(vec![
                Value::Int64(1),
                Value::Float64(2.0),
                Value::Bool(false),
                Value::Int64(3),
            ])
            .unwrap_err();

        assert_eq!(
            error,
            TableError::TypeMismatch {
                column: 3,
                field: "string".to_owned(),
                expected: DataType::String,
                actual: DataType::Int64,
            }
        );
        assert_eq!(table, before);
    }
}
