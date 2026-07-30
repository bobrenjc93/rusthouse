use crate::error::{Error, Result};
use crate::sql::{ArithmeticOperator, Expression, ScalarFunction};
use crate::storage::Table;
use crate::value::{DataType, Value, ValueRef};

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CompiledExpression {
    Literal(Value),
    Column {
        index: usize,
        data_type: DataType,
    },
    UnaryMinus {
        expression: Box<Self>,
        data_type: DataType,
    },
    Binary {
        left: Box<Self>,
        operator: ArithmeticOperator,
        right: Box<Self>,
        data_type: DataType,
    },
    Cast {
        expression: Box<Self>,
        target: DataType,
    },
    Function {
        function: ScalarFunction,
        arguments: Vec<Self>,
        data_type: DataType,
    },
}

pub(crate) enum Evaluated<'a> {
    Borrowed(ValueRef<'a>),
    Owned(Value),
}

impl Evaluated<'_> {
    pub(crate) fn as_ref(&self) -> ValueRef<'_> {
        match self {
            Self::Borrowed(value) => *value,
            Self::Owned(value) => value.as_ref(),
        }
    }

    pub(crate) fn into_owned(self) -> Value {
        match self {
            Self::Borrowed(value) => value.to_owned(),
            Self::Owned(value) => value,
        }
    }
}

impl CompiledExpression {
    pub(crate) fn compile(table: &Table, expression: &Expression) -> Result<Self> {
        let compiled = match expression {
            Expression::Literal(value) => Self::Literal(value.clone()),
            Expression::Column(name) => {
                let index = table.column_index(name)?;
                Self::Column {
                    index,
                    data_type: table.schema()[index].data_type,
                }
            }
            Expression::UnaryMinus(expression) => {
                let expression = Self::compile(table, expression)?;
                let data_type = expression.data_type();
                require_numeric(data_type, "unary '-' expression")?;
                Self::UnaryMinus {
                    expression: Box::new(expression),
                    data_type,
                }
            }
            Expression::Binary {
                left,
                operator,
                right,
            } => {
                let left = Self::compile(table, left)?;
                let right = Self::compile(table, right)?;
                let left_type = left.data_type();
                let right_type = right.data_type();
                require_numeric(left_type, &format!("left operand of '{operator}'"))?;
                require_numeric(right_type, &format!("right operand of '{operator}'"))?;
                let data_type = if *operator == ArithmeticOperator::Divide
                    || left_type == DataType::Float64
                    || right_type == DataType::Float64
                {
                    DataType::Float64
                } else {
                    DataType::Int64
                };
                Self::Binary {
                    left: Box::new(left),
                    operator: *operator,
                    right: Box::new(right),
                    data_type,
                }
            }
            Expression::Cast { expression, target } => Self::Cast {
                expression: Box::new(Self::compile(table, expression)?),
                target: *target,
            },
            Expression::Function {
                function,
                arguments,
            } => {
                let arguments = arguments
                    .iter()
                    .map(|argument| Self::compile(table, argument))
                    .collect::<Result<Vec<_>>>()?;
                let data_type = resolve_function_type(*function, &arguments)?;
                Self::Function {
                    function: *function,
                    arguments,
                    data_type,
                }
            }
        };

        if compiled.has_columns() {
            Ok(compiled)
        } else {
            let value = compiled.evaluate(table, None)?.into_owned();
            Ok(Self::Literal(value))
        }
    }

    pub(crate) fn data_type(&self) -> DataType {
        match self {
            Self::Literal(value) => value.data_type(),
            Self::Column { data_type, .. }
            | Self::UnaryMinus { data_type, .. }
            | Self::Binary { data_type, .. }
            | Self::Function { data_type, .. } => *data_type,
            Self::Cast { target, .. } => *target,
        }
    }

    pub(crate) fn evaluate<'a>(
        &'a self,
        table: &'a Table,
        row: Option<usize>,
    ) -> Result<Evaluated<'a>> {
        match self {
            Self::Literal(value) => Ok(Evaluated::Borrowed(value.as_ref())),
            Self::Column { index, .. } => {
                let row = row.expect("column expressions are evaluated against a source row");
                Ok(Evaluated::Borrowed(table.columns()[*index].value_ref(row)))
            }
            Self::UnaryMinus { expression, .. } => {
                let value = expression.evaluate(table, row)?;
                evaluate_unary_minus(value.as_ref()).map(Evaluated::Owned)
            }
            Self::Binary {
                left,
                operator,
                right,
                data_type,
            } => {
                let left = left.evaluate(table, row)?;
                let right = right.evaluate(table, row)?;
                evaluate_binary(left.as_ref(), *operator, right.as_ref(), *data_type)
                    .map(Evaluated::Owned)
            }
            Self::Cast { expression, target } => {
                let value = expression.evaluate(table, row)?;
                cast_value(value.as_ref(), *target).map(Evaluated::Owned)
            }
            Self::Function {
                function,
                arguments,
                ..
            } => {
                let values = arguments
                    .iter()
                    .map(|argument| argument.evaluate(table, row))
                    .collect::<Result<Vec<_>>>()?;
                evaluate_function(*function, &values).map(Evaluated::Owned)
            }
        }
    }

    pub(crate) fn for_each_column(&self, visitor: &mut impl FnMut(usize)) {
        match self {
            Self::Literal(_) => {}
            Self::Column { index, .. } => visitor(*index),
            Self::UnaryMinus { expression, .. } => expression.for_each_column(visitor),
            Self::Binary { left, right, .. } => {
                left.for_each_column(visitor);
                right.for_each_column(visitor);
            }
            Self::Cast { expression, .. } => expression.for_each_column(visitor),
            Self::Function { arguments, .. } => {
                for argument in arguments {
                    argument.for_each_column(visitor);
                }
            }
        }
    }

    pub(crate) fn for_each_uncovered_column(
        &self,
        covered: &[Self],
        visitor: &mut impl FnMut(usize),
    ) {
        if covered.contains(self) {
            return;
        }
        match self {
            Self::Literal(_) => {}
            Self::Column { index, .. } => visitor(*index),
            Self::UnaryMinus { expression, .. } | Self::Cast { expression, .. } => {
                expression.for_each_uncovered_column(covered, visitor);
            }
            Self::Binary { left, right, .. } => {
                left.for_each_uncovered_column(covered, visitor);
                right.for_each_uncovered_column(covered, visitor);
            }
            Self::Function { arguments, .. } => {
                for argument in arguments {
                    argument.for_each_uncovered_column(covered, visitor);
                }
            }
        }
    }

    fn has_columns(&self) -> bool {
        let mut has_columns = false;
        self.for_each_column(&mut |_| has_columns = true);
        has_columns
    }
}

fn resolve_function_type(
    function: ScalarFunction,
    arguments: &[CompiledExpression],
) -> Result<DataType> {
    let types = arguments
        .iter()
        .map(CompiledExpression::data_type)
        .collect::<Vec<_>>();
    match function {
        ScalarFunction::Abs => {
            require_numeric(types[0], "ABS argument")?;
            Ok(types[0])
        }
        ScalarFunction::Round => {
            require_numeric(types[0], "ROUND value argument")?;
            if let Some(precision) = types.get(1) {
                require_type(*precision, DataType::Int64, "ROUND precision argument")?;
            }
            Ok(types[0])
        }
        ScalarFunction::Lower | ScalarFunction::Upper => {
            require_type(
                types[0],
                DataType::String,
                &format!("{} argument", function.name()),
            )?;
            Ok(DataType::String)
        }
        ScalarFunction::Length => {
            require_type(types[0], DataType::String, "LENGTH argument")?;
            Ok(DataType::Int64)
        }
        ScalarFunction::Substring => {
            require_type(types[0], DataType::String, "SUBSTRING string argument")?;
            require_type(types[1], DataType::Int64, "SUBSTRING start argument")?;
            if let Some(length) = types.get(2) {
                require_type(*length, DataType::Int64, "SUBSTRING length argument")?;
            }
            Ok(DataType::String)
        }
    }
}

fn require_type(actual: DataType, expected: DataType, context: &str) -> Result<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(Error::TypeMismatch {
            context: context.to_owned(),
            expected: expected.to_string(),
            actual: actual.to_string(),
        })
    }
}

fn require_numeric(data_type: DataType, context: &str) -> Result<()> {
    if matches!(data_type, DataType::Int64 | DataType::Float64) {
        Ok(())
    } else {
        Err(Error::TypeMismatch {
            context: context.to_owned(),
            expected: "Int64 or Float64".to_owned(),
            actual: data_type.to_string(),
        })
    }
}

fn evaluate_unary_minus(value: ValueRef<'_>) -> Result<Value> {
    match value {
        ValueRef::Int64(value) => value
            .checked_neg()
            .map(Value::Int64)
            .ok_or_else(|| Error::NumericOverflow("unary '-' on Int64".to_owned())),
        ValueRef::Float64(value) => finite_float(-value, "unary '-' on Float64"),
        ValueRef::Bool(_) | ValueRef::String(_) => {
            unreachable!("unary expression types are resolved before evaluation")
        }
    }
}

fn evaluate_binary(
    left: ValueRef<'_>,
    operator: ArithmeticOperator,
    right: ValueRef<'_>,
    data_type: DataType,
) -> Result<Value> {
    if operator == ArithmeticOperator::Divide && is_zero(right) {
        return Err(Error::DivisionByZero("'/' expression".to_owned()));
    }

    match data_type {
        DataType::Int64 => {
            let (ValueRef::Int64(left), ValueRef::Int64(right)) = (left, right) else {
                unreachable!("Int64 arithmetic input types are resolved")
            };
            let result = match operator {
                ArithmeticOperator::Add => left.checked_add(right),
                ArithmeticOperator::Subtract => left.checked_sub(right),
                ArithmeticOperator::Multiply => left.checked_mul(right),
                ArithmeticOperator::Divide => unreachable!("division always returns Float64"),
            };
            result
                .map(Value::Int64)
                .ok_or_else(|| Error::NumericOverflow(format!("Int64 '{operator}' expression")))
        }
        DataType::Float64 => {
            let left = as_float(left);
            let right = as_float(right);
            let result = match operator {
                ArithmeticOperator::Add => left + right,
                ArithmeticOperator::Subtract => left - right,
                ArithmeticOperator::Multiply => left * right,
                ArithmeticOperator::Divide => left / right,
            };
            finite_float(result, &format!("Float64 '{operator}' expression"))
        }
        DataType::Bool | DataType::String => {
            unreachable!("arithmetic result types are resolved before evaluation")
        }
    }
}

fn is_zero(value: ValueRef<'_>) -> bool {
    matches!(value, ValueRef::Int64(0)) || matches!(value, ValueRef::Float64(value) if value == 0.0)
}

fn as_float(value: ValueRef<'_>) -> f64 {
    match value {
        ValueRef::Int64(value) => value as f64,
        ValueRef::Float64(value) => value,
        ValueRef::Bool(_) | ValueRef::String(_) => {
            unreachable!("Float64 arithmetic input types are resolved")
        }
    }
}

fn finite_float(value: f64, operation: &str) -> Result<Value> {
    if value.is_finite() {
        Ok(Value::Float64(value))
    } else {
        Err(Error::NonFiniteResult(operation.to_owned()))
    }
}

fn cast_value(value: ValueRef<'_>, target: DataType) -> Result<Value> {
    let source = value.data_type();
    let converted = match target {
        DataType::Int64 => cast_to_int(value).map(Value::Int64),
        DataType::Float64 => cast_to_float(value).map(Value::Float64),
        DataType::Bool => cast_to_bool(value).map(Value::Bool),
        DataType::String => Ok(Value::String(value.as_display_string())),
    };
    converted.map_err(|()| Error::CastFailed {
        value: value.as_display_string(),
        source,
        target,
    })
}

fn cast_to_int(value: ValueRef<'_>) -> std::result::Result<i64, ()> {
    const I64_UPPER_EXCLUSIVE: f64 = 9_223_372_036_854_775_808.0;

    match value {
        ValueRef::Int64(value) => Ok(value),
        ValueRef::Float64(value)
            if value.is_finite() && value >= i64::MIN as f64 && value < I64_UPPER_EXCLUSIVE =>
        {
            Ok(value.trunc() as i64)
        }
        ValueRef::Float64(_) => Err(()),
        ValueRef::Bool(value) => Ok(i64::from(value)),
        ValueRef::String(value) => value.trim().parse().map_err(|_| ()),
    }
}

fn cast_to_float(value: ValueRef<'_>) -> std::result::Result<f64, ()> {
    let converted = match value {
        ValueRef::Int64(value) => value as f64,
        ValueRef::Float64(value) => value,
        ValueRef::Bool(value) => f64::from(u8::from(value)),
        ValueRef::String(value) => value.trim().parse().map_err(|_| ())?,
    };
    converted.is_finite().then_some(converted).ok_or(())
}

fn cast_to_bool(value: ValueRef<'_>) -> std::result::Result<bool, ()> {
    match value {
        ValueRef::Int64(value) => Ok(value != 0),
        ValueRef::Float64(value) if value.is_finite() => Ok(value != 0.0),
        ValueRef::Float64(_) => Err(()),
        ValueRef::Bool(value) => Ok(value),
        ValueRef::String(value) => match value.trim().to_ascii_lowercase().as_str() {
            "true" | "1" => Ok(true),
            "false" | "0" => Ok(false),
            _ => Err(()),
        },
    }
}

fn evaluate_function(function: ScalarFunction, arguments: &[Evaluated<'_>]) -> Result<Value> {
    match function {
        ScalarFunction::Abs => evaluate_abs(arguments[0].as_ref()),
        ScalarFunction::Round => {
            let precision = arguments.get(1).map_or(0, |value| {
                let ValueRef::Int64(precision) = value.as_ref() else {
                    unreachable!("ROUND precision type is resolved")
                };
                precision
            });
            evaluate_round(arguments[0].as_ref(), precision)
        }
        ScalarFunction::Lower => {
            let ValueRef::String(value) = arguments[0].as_ref() else {
                unreachable!("LOWER argument type is resolved")
            };
            Ok(Value::String(value.to_lowercase()))
        }
        ScalarFunction::Upper => {
            let ValueRef::String(value) = arguments[0].as_ref() else {
                unreachable!("UPPER argument type is resolved")
            };
            Ok(Value::String(value.to_uppercase()))
        }
        ScalarFunction::Length => {
            let ValueRef::String(value) = arguments[0].as_ref() else {
                unreachable!("LENGTH argument type is resolved")
            };
            let length = i64::try_from(value.chars().count())
                .map_err(|_| Error::NumericOverflow("LENGTH result".to_owned()))?;
            Ok(Value::Int64(length))
        }
        ScalarFunction::Substring => {
            let ValueRef::String(value) = arguments[0].as_ref() else {
                unreachable!("SUBSTRING string type is resolved")
            };
            let ValueRef::Int64(start) = arguments[1].as_ref() else {
                unreachable!("SUBSTRING start type is resolved")
            };
            let length = arguments.get(2).map(|value| {
                let ValueRef::Int64(length) = value.as_ref() else {
                    unreachable!("SUBSTRING length type is resolved")
                };
                length
            });
            evaluate_substring(value, start, length).map(Value::String)
        }
    }
}

fn evaluate_abs(value: ValueRef<'_>) -> Result<Value> {
    match value {
        ValueRef::Int64(value) => value
            .checked_abs()
            .map(Value::Int64)
            .ok_or_else(|| Error::NumericOverflow("ABS(Int64)".to_owned())),
        ValueRef::Float64(value) => finite_float(value.abs(), "ABS(Float64)"),
        ValueRef::Bool(_) | ValueRef::String(_) => {
            unreachable!("ABS argument type is resolved")
        }
    }
}

fn evaluate_round(value: ValueRef<'_>, precision: i64) -> Result<Value> {
    match value {
        ValueRef::Int64(value) => round_int(value, precision).map(Value::Int64),
        ValueRef::Float64(value) => round_float(value, precision),
        ValueRef::Bool(_) | ValueRef::String(_) => {
            unreachable!("ROUND argument type is resolved")
        }
    }
}

fn round_int(value: i64, precision: i64) -> Result<i64> {
    if precision >= 0 {
        return Ok(value);
    }
    let digits = precision.unsigned_abs();
    if digits > 19 {
        return Ok(0);
    }
    let factor = 10_i128.pow(u32::try_from(digits).expect("digits are at most 19"));
    let value = i128::from(value);
    let mut rounded = value / factor;
    let remainder = value % factor;
    if remainder.abs() * 2 >= factor {
        rounded += value.signum();
    }
    i64::try_from(rounded * factor).map_err(|_| Error::NumericOverflow("ROUND(Int64)".to_owned()))
}

fn round_float(value: f64, precision: i64) -> Result<Value> {
    let result = if precision > 308 {
        value
    } else if precision < -308 {
        0.0_f64.copysign(value)
    } else if precision >= 0 {
        let factor = 10_f64.powi(precision as i32);
        let scaled = value * factor;
        if scaled.is_finite() {
            scaled.round() / factor
        } else {
            value
        }
    } else {
        let factor = 10_f64.powi((-precision) as i32);
        (value / factor).round() * factor
    };
    finite_float(result, "ROUND(Float64)")
}

fn evaluate_substring(value: &str, start: i64, length: Option<i64>) -> Result<String> {
    let Some(length) = length else {
        return substring_with_limit(value, start, None);
    };
    if length < 0 {
        return Err(Error::InvalidQuery(
            "SUBSTRING length must be non-negative".to_owned(),
        ));
    }
    substring_with_limit(value, start, usize::try_from(length).ok())
}

fn substring_with_limit(value: &str, start: i64, length: Option<usize>) -> Result<String> {
    let char_count = value.chars().count();
    let index = match start.cmp(&0) {
        std::cmp::Ordering::Greater => {
            let offset = usize::try_from(start - 1).unwrap_or(usize::MAX);
            if offset >= char_count {
                return Ok(String::new());
            }
            offset
        }
        std::cmp::Ordering::Equal => return Ok(String::new()),
        std::cmp::Ordering::Less => {
            let distance = usize::try_from(start.unsigned_abs()).unwrap_or(usize::MAX);
            if distance > char_count {
                return Ok(String::new());
            }
            char_count - distance
        }
    };
    Ok(value
        .chars()
        .skip(index)
        .take(length.unwrap_or(usize::MAX))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql;
    use crate::storage::ColumnDef;

    fn table() -> Table {
        Table::new(
            "numbers".to_owned(),
            vec![ColumnDef {
                name: "n".to_owned(),
                data_type: DataType::Int64,
            }],
        )
        .expect("valid table")
    }

    fn select_expression(sql: &str) -> Expression {
        let sql::Statement::Select(select) = sql::parse(sql).expect("valid SQL").remove(0) else {
            panic!("expected SELECT")
        };
        let sql::SelectItem::Expression { expression, .. } =
            select.items.into_iter().next().unwrap()
        else {
            panic!("expected expression")
        };
        expression
    }

    #[test]
    fn constant_folding_uses_precedence_and_division_type() {
        let table = table();
        let expression = select_expression("SELECT (2 + 3) * 4 / 2 FROM numbers");
        let compiled = CompiledExpression::compile(&table, &expression).expect("compile");

        assert_eq!(compiled.data_type(), DataType::Float64);
        assert_eq!(
            compiled.evaluate(&table, None).unwrap().into_owned(),
            Value::Float64(10.0)
        );
    }
}
