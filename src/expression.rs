use crate::error::{Error, Result};
use crate::sql::{ArithmeticOperator, Expression};
use crate::storage::Table;
use crate::value::{DataType, Value, ValueRef};

#[derive(Debug, Clone)]
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
            | Self::Binary { data_type, .. } => *data_type,
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
        }
    }

    fn has_columns(&self) -> bool {
        let mut has_columns = false;
        self.for_each_column(&mut |_| has_columns = true);
        has_columns
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
