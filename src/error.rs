use std::fmt;

/// Errors returned by the columnar batch layer and its execution kernels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    InvalidCapacity {
        capacity: usize,
    },
    CapacityExceeded {
        capacity: usize,
    },
    CapacityMismatch {
        column: usize,
        expected: usize,
        actual: usize,
    },
    LengthMismatch {
        column: usize,
        expected: usize,
        actual: usize,
    },
    SchemaMismatch {
        fields: usize,
        columns: usize,
    },
    TypeMismatch {
        column: usize,
        expected: &'static str,
        actual: &'static str,
    },
    NullInNonNullableColumn {
        column: usize,
    },
    InvalidColumn {
        column: usize,
        columns: usize,
    },
    SelectionMismatch {
        expected_len: usize,
        actual_len: usize,
        expected_capacity: usize,
        actual_capacity: usize,
    },
    UnsupportedAggregate {
        aggregate: &'static str,
        data_type: &'static str,
    },
    InvalidAggregate {
        aggregate: &'static str,
        reason: &'static str,
    },
    ArithmeticOverflow {
        aggregate: &'static str,
    },
    MemoryLimitExceeded {
        operator: &'static str,
        required: usize,
        limit: usize,
    },
    GroupLimitExceeded {
        max_groups: usize,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCapacity { capacity } => {
                write!(f, "capacity {capacity} is not representable by this array")
            }
            Self::CapacityExceeded { capacity } => {
                write!(f, "fixed array capacity {capacity} exceeded")
            }
            Self::CapacityMismatch {
                column,
                expected,
                actual,
            } => write!(
                f,
                "column {column} has capacity {actual}, expected {expected}"
            ),
            Self::LengthMismatch {
                column,
                expected,
                actual,
            } => write!(
                f,
                "column {column} has length {actual}, expected {expected}"
            ),
            Self::SchemaMismatch { fields, columns } => {
                write!(
                    f,
                    "schema has {fields} fields but batch has {columns} columns"
                )
            }
            Self::TypeMismatch {
                column,
                expected,
                actual,
            } => write!(f, "column {column} has type {actual}, expected {expected}"),
            Self::NullInNonNullableColumn { column } => {
                write!(f, "non-nullable column {column} contains NULL")
            }
            Self::InvalidColumn { column, columns } => {
                write!(f, "column {column} is out of bounds for {columns} columns")
            }
            Self::SelectionMismatch {
                expected_len,
                actual_len,
                expected_capacity,
                actual_capacity,
            } => write!(
                f,
                "selection has len/capacity {actual_len}/{actual_capacity}, expected {expected_len}/{expected_capacity}"
            ),
            Self::UnsupportedAggregate {
                aggregate,
                data_type,
            } => write!(f, "{aggregate} does not support {data_type}"),
            Self::InvalidAggregate { aggregate, reason } => {
                write!(f, "invalid {aggregate} aggregate: {reason}")
            }
            Self::ArithmeticOverflow { aggregate } => {
                write!(f, "{aggregate} overflowed its accumulator")
            }
            Self::MemoryLimitExceeded {
                operator,
                required,
                limit,
            } => write!(
                f,
                "{operator} requires {required} retained bytes, limit is {limit}"
            ),
            Self::GroupLimitExceeded { max_groups } => {
                write!(f, "hash grouping exceeded its {max_groups} group limit")
            }
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
