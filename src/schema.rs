//! Table schema definitions and validation.

use std::collections::HashSet;
use std::error::Error;
use std::fmt;

use crate::scalar::DataType;

/// A named, typed field in a [`Schema`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    name: String,
    data_type: DataType,
}

impl Field {
    /// Creates a field. Field names are validated when a [`Schema`] is built.
    pub fn new(name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            name: name.into(),
            data_type,
        }
    }

    /// Returns the field name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the field's scalar type.
    #[must_use]
    pub const fn data_type(&self) -> DataType {
        self.data_type
    }
}

/// An ordered collection of uniquely named fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Schema {
    fields: Vec<Field>,
}

impl Schema {
    /// Builds a schema and validates all field names.
    ///
    /// Names must contain a non-whitespace character and must be unique under
    /// case-insensitive comparison. A schema must contain at least one field.
    pub fn new(fields: Vec<Field>) -> Result<Self, SchemaError> {
        if fields.is_empty() {
            return Err(SchemaError::EmptySchema);
        }

        let mut names = HashSet::with_capacity(fields.len());
        for (index, field) in fields.iter().enumerate() {
            if field.name.trim().is_empty() {
                return Err(SchemaError::EmptyFieldName { index });
            }

            if !names.insert(field.name.to_lowercase()) {
                return Err(SchemaError::DuplicateFieldName {
                    name: field.name.clone(),
                });
            }
        }

        Ok(Self { fields })
    }

    /// Returns the fields in their storage order.
    #[must_use]
    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    /// Returns the number of fields.
    #[must_use]
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// Returns whether the schema has no fields.
    ///
    /// Valid schemas are never empty; this method is supplied for collection
    /// API consistency.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// Finds a field by case-insensitive name.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields
            .iter()
            .find(|field| field.name.eq_ignore_ascii_case(name))
    }

    /// Finds a field's storage index by case-insensitive name.
    #[must_use]
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.fields
            .iter()
            .position(|field| field.name.eq_ignore_ascii_case(name))
    }
}

impl TryFrom<Vec<Field>> for Schema {
    type Error = SchemaError;

    fn try_from(fields: Vec<Field>) -> Result<Self, Self::Error> {
        Self::new(fields)
    }
}

/// A schema validation failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SchemaError {
    /// The schema did not contain any fields.
    EmptySchema,
    /// A field name was empty or contained only whitespace.
    EmptyFieldName { index: usize },
    /// A field name duplicated an earlier name under case-insensitive comparison.
    DuplicateFieldName { name: String },
}

impl fmt::Display for SchemaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySchema => formatter.write_str("a schema must contain at least one field"),
            Self::EmptyFieldName { index } => {
                write!(formatter, "field at index {index} has an empty name")
            }
            Self::DuplicateFieldName { name } => {
                write!(formatter, "field name `{name}` is duplicated")
            }
        }
    }
}

impl Error for SchemaError {}
