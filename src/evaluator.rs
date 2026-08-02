//! Typed scalar expression evaluation.

use crate::ScalarValue;

#[derive(Clone, Copy)]
pub(crate) enum ComparisonOperator {
    Equal,
    NotEqual,
    LessThan,
    LessThanOrEqual,
    GreaterThan,
    GreaterThanOrEqual,
}

impl ComparisonOperator {
    fn sql(self) -> &'static str {
        match self {
            Self::Equal => "=",
            Self::NotEqual => "<>",
            Self::LessThan => "<",
            Self::LessThanOrEqual => "<=",
            Self::GreaterThan => ">",
            Self::GreaterThanOrEqual => ">=",
        }
    }

    fn apply(self, ordering: std::cmp::Ordering) -> bool {
        use std::cmp::Ordering::{Equal, Greater, Less};

        match self {
            Self::Equal => ordering == Equal,
            Self::NotEqual => ordering != Equal,
            Self::LessThan => ordering == Less,
            Self::LessThanOrEqual => matches!(ordering, Less | Equal),
            Self::GreaterThan => ordering == Greater,
            Self::GreaterThanOrEqual => matches!(ordering, Greater | Equal),
        }
    }
}

pub(crate) fn compare_scalars(
    left: ScalarValue,
    right: ScalarValue,
    operator: ComparisonOperator,
) -> Result<ScalarValue, String> {
    let ordering = match (left, right) {
        (ScalarValue::Null, _) | (_, ScalarValue::Null) => return Ok(ScalarValue::Null),
        (ScalarValue::Integer(left), ScalarValue::Integer(right)) => left.cmp(&right),
        (ScalarValue::Float(left), ScalarValue::Float(right)) => left
            .partial_cmp(&right)
            .expect("SQL float literals are finite"),
        (ScalarValue::Boolean(left), ScalarValue::Boolean(right)) => left.cmp(&right),
        (ScalarValue::String(left), ScalarValue::String(right)) => left.cmp(&right),
        (left, right) => {
            return Err(format!(
                "operator '{}' cannot compare {} and {}",
                operator.sql(),
                scalar_type_name(&left),
                scalar_type_name(&right)
            ));
        }
    };

    Ok(ScalarValue::Boolean(operator.apply(ordering)))
}

fn scalar_type_name(value: &ScalarValue) -> &'static str {
    match value {
        ScalarValue::Null => "NULL",
        ScalarValue::Integer(_) => "Integer",
        ScalarValue::Float(_) => "Float",
        ScalarValue::Boolean(_) => "Boolean",
        ScalarValue::String(_) => "String",
    }
}
