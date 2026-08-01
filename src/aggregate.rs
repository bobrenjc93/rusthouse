use std::{cmp::Ordering, fmt, str::FromStr};

use crate::{Error, Result, Value, value::compare_int_float};

/// Built-in aggregate functions. `CountAll` models `COUNT(*)`; `Count` models
/// `COUNT(expression)` and therefore ignores NULL inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateFunction {
    Count,
    CountAll,
    Sum,
    Min,
    Max,
    Avg,
}

impl FromStr for AggregateFunction {
    type Err = Error;

    fn from_str(name: &str) -> Result<Self> {
        match name.to_ascii_lowercase().as_str() {
            "count" => Ok(Self::Count),
            "count_all" => Ok(Self::CountAll),
            "sum" => Ok(Self::Sum),
            "min" => Ok(Self::Min),
            "max" => Ok(Self::Max),
            "avg" => Ok(Self::Avg),
            _ => Err(Error::Aggregate(format!(
                "unknown aggregate function `{name}`"
            ))),
        }
    }
}

impl fmt::Display for AggregateFunction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Count => "count",
            Self::CountAll => "count_all",
            Self::Sum => "sum",
            Self::Min => "min",
            Self::Max => "max",
            Self::Avg => "avg",
        })
    }
}

#[derive(Debug, Clone, Default)]
struct SumState {
    integer: i128,
    float: f64,
    saw_integer: bool,
    saw_float: bool,
}

#[derive(Debug, Clone)]
enum State {
    Count(u64),
    Sum(SumState),
    Min(Option<Value>),
    Max(Option<Value>),
    Avg { sum: f64, count: u64 },
}

/// Mutable state for one aggregate group.
#[derive(Debug, Clone)]
pub struct Aggregate {
    function: AggregateFunction,
    state: State,
}

impl Aggregate {
    pub fn new(function: AggregateFunction) -> Self {
        let state = match function {
            AggregateFunction::Count | AggregateFunction::CountAll => State::Count(0),
            AggregateFunction::Sum => State::Sum(SumState::default()),
            AggregateFunction::Min => State::Min(None),
            AggregateFunction::Max => State::Max(None),
            AggregateFunction::Avg => State::Avg { sum: 0.0, count: 0 },
        };
        Self { function, state }
    }

    pub const fn function(&self) -> AggregateFunction {
        self.function
    }

    /// Adds one input row. NULL is ignored except by `CountAll`, which counts
    /// rows rather than expression values.
    pub fn add(&mut self, value: &Value) -> Result<()> {
        match (&mut self.state, self.function) {
            (State::Count(count), AggregateFunction::Count) => {
                if !value.is_null() {
                    *count = increment_count(*count)?;
                }
                Ok(())
            }
            (State::Count(count), AggregateFunction::CountAll) => {
                *count = increment_count(*count)?;
                Ok(())
            }
            (State::Sum(sum), AggregateFunction::Sum) => add_sum(sum, value),
            (State::Min(current), AggregateFunction::Min) => add_extreme(current, value, true),
            (State::Max(current), AggregateFunction::Max) => add_extreme(current, value, false),
            (State::Avg { sum, count }, AggregateFunction::Avg) => {
                if value.is_null() {
                    return Ok(());
                }
                let numeric = match value {
                    Value::Int64(value) => *value as f64,
                    Value::Float64(value) => *value,
                    value => return Err(aggregate_type_error("avg", value)),
                };
                let new_count = increment_count(*count)?;
                *sum += numeric;
                *count = new_count;
                Ok(())
            }
            _ => unreachable!("aggregate function and state are created together"),
        }
    }

    /// Returns the current result without consuming the state. COUNT returns
    /// zero for empty input; every other aggregate returns NULL when no
    /// non-NULL value was observed.
    pub fn finish(&self) -> Result<Value> {
        match &self.state {
            State::Count(count) => {
                i64::try_from(*count)
                    .map(Value::Int64)
                    .map_err(|_| Error::Overflow {
                        operation: self.function.to_string(),
                    })
            }
            State::Sum(sum) if !sum.saw_integer && !sum.saw_float => Ok(Value::Null),
            State::Sum(sum) if sum.saw_float => Ok(Value::Float64(sum.integer as f64 + sum.float)),
            State::Sum(sum) => {
                i64::try_from(sum.integer)
                    .map(Value::Int64)
                    .map_err(|_| Error::Overflow {
                        operation: "sum".to_owned(),
                    })
            }
            State::Min(None) | State::Max(None) => Ok(Value::Null),
            State::Min(Some(value)) | State::Max(Some(value)) => Ok(value.clone()),
            State::Avg { count: 0, .. } => Ok(Value::Null),
            State::Avg { sum, count } => Ok(Value::Float64(*sum / *count as f64)),
        }
    }
}

fn increment_count(count: u64) -> Result<u64> {
    count.checked_add(1).ok_or_else(|| Error::Overflow {
        operation: "aggregate row count".to_owned(),
    })
}

fn add_sum(state: &mut SumState, value: &Value) -> Result<()> {
    match value {
        Value::Null => Ok(()),
        Value::Int64(value) => {
            state.integer = state
                .integer
                .checked_add(i128::from(*value))
                .ok_or_else(|| Error::Overflow {
                    operation: "sum".to_owned(),
                })?;
            state.saw_integer = true;
            Ok(())
        }
        Value::Float64(value) => {
            state.float += value;
            state.saw_float = true;
            Ok(())
        }
        value => Err(aggregate_type_error("sum", value)),
    }
}

fn add_extreme(current: &mut Option<Value>, value: &Value, minimum: bool) -> Result<()> {
    if value.is_null() {
        return Ok(());
    }
    if let Some(current) = current.as_ref()
        && !comparable_domains(value, current)
    {
        return Err(Error::Aggregate(format!(
            "cannot compare {} with {} in min/max",
            value.type_name(),
            current.type_name()
        )));
    }
    if is_nan(value) || current.as_ref().is_some_and(is_nan) {
        *current = Some(Value::Float64(f64::NAN));
        return Ok(());
    }
    let replace = match current {
        None => true,
        Some(current) => {
            let ordering = aggregate_compare(value, current)?;
            match ordering {
                Ordering::Less => minimum,
                Ordering::Greater => !minimum,
                Ordering::Equal => matches!((value, current), (Value::Float64(_), Value::Int64(_))),
            }
        }
    };
    if replace {
        *current = Some(value.clone());
    }
    Ok(())
}

fn comparable_domains(left: &Value, right: &Value) -> bool {
    matches!(
        (left, right),
        (
            Value::Int64(_) | Value::Float64(_),
            Value::Int64(_) | Value::Float64(_)
        ) | (Value::Bool(_), Value::Bool(_))
            | (Value::String(_), Value::String(_))
    )
}

fn aggregate_compare(left: &Value, right: &Value) -> Result<Ordering> {
    match (left, right) {
        (Value::Int64(left), Value::Int64(right)) => Ok(left.cmp(right)),
        (Value::Int64(left), Value::Float64(right)) => compare_int_float(*left, *right)
            .ok_or_else(|| Error::Aggregate("NaN is unordered".to_owned())),
        (Value::Float64(left), Value::Int64(right)) => compare_int_float(*right, *left)
            .map(Ordering::reverse)
            .ok_or_else(|| Error::Aggregate("NaN is unordered".to_owned())),
        (Value::Float64(left), Value::Float64(right)) => left
            .partial_cmp(right)
            .ok_or_else(|| Error::Aggregate("NaN is unordered".to_owned())),
        (Value::Bool(left), Value::Bool(right)) => Ok(left.cmp(right)),
        (Value::String(left), Value::String(right)) => Ok(left.cmp(right)),
        _ => Err(Error::Aggregate(format!(
            "cannot compare {} with {} in min/max",
            left.type_name(),
            right.type_name()
        ))),
    }
}

fn is_nan(value: &Value) -> bool {
    matches!(value, Value::Float64(value) if value.is_nan())
}

fn aggregate_type_error(function: &str, value: &Value) -> Error {
    Error::Aggregate(format!(
        "{function} expects numeric or NULL values, got {}",
        value.type_name()
    ))
}
