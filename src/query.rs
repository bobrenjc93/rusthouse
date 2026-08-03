//! Typed planning and materialized execution for nontrivial `SELECT` queries.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;

use crate::scan::RowSelection;
use crate::sql::{
    AggregateFunction, OrderByExpression, SelectExpression, SelectItem, SelectProjection,
    SelectStatement,
};
use crate::{DataType, Field, Table, TableError, Value};

/// A deterministic planning or execution failure for a materialized query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryError {
    FieldNotFound {
        name: String,
    },
    NonNumericAggregate {
        function: AggregateFunction,
        field: String,
        data_type: DataType,
    },
    UngroupedColumn {
        name: String,
    },
    DuplicateGroupField {
        name: String,
    },
    DuplicateOutputField {
        name: String,
    },
    OrderFieldNotFound {
        name: String,
    },
    SelectionLengthMismatch {
        table_rows: usize,
        selection_rows: usize,
    },
    SourceSchemaMismatch,
    Int64Overflow {
        function: AggregateFunction,
        field: String,
    },
    ResultConstruction(TableError),
}

impl fmt::Display for QueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FieldNotFound { name } => write!(formatter, "field `{name}` does not exist"),
            Self::NonNumericAggregate {
                function,
                field,
                data_type,
            } => write!(
                formatter,
                "{} requires a numeric field; `{field}` has type {data_type}",
                function.as_str()
            ),
            Self::UngroupedColumn { name } => write!(
                formatter,
                "projected field `{name}` must appear in GROUP BY or be aggregated"
            ),
            Self::DuplicateGroupField { name } => {
                write!(formatter, "GROUP BY field `{name}` is repeated")
            }
            Self::DuplicateOutputField { name } => {
                write!(formatter, "query output field `{name}` is repeated")
            }
            Self::OrderFieldNotFound { name } => {
                write!(formatter, "ORDER BY field `{name}` is not projected")
            }
            Self::SelectionLengthMismatch {
                table_rows,
                selection_rows,
            } => write!(
                formatter,
                "selection represents {selection_rows} rows; table contains {table_rows} rows"
            ),
            Self::SourceSchemaMismatch => {
                formatter.write_str("query plan was executed against a different source schema")
            }
            Self::Int64Overflow { function, field } => write!(
                formatter,
                "{} over Int64 field `{field}` overflowed",
                function.as_str()
            ),
            Self::ResultConstruction(error) => {
                write!(formatter, "could not construct query result: {error}")
            }
        }
    }
}

impl Error for QueryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ResultConstruction(error) => Some(error),
            _ => None,
        }
    }
}

/// A resolved query plan whose field lookups and result types are validated.
#[derive(Clone, Debug)]
pub struct QueryPlan {
    source_fields: Vec<Field>,
    fields: Vec<Field>,
    projections: Vec<PlannedProjection>,
    group_fields: Vec<usize>,
    aggregates: Vec<AggregateSpec>,
    order_by: Vec<PlannedOrder>,
    limit: Option<usize>,
    grouped: bool,
}

impl QueryPlan {
    /// Resolves a parsed statement against one source table.
    pub fn build(table: &Table, statement: &SelectStatement) -> Result<Self, QueryError> {
        let items = projection_items(table, &statement.projections);
        let grouped = !statement.group_by.is_empty()
            || items
                .iter()
                .any(|item| matches!(item.expression, SelectExpression::Aggregate { .. }));

        let mut group_fields = Vec::new();
        let mut seen_group_fields = HashSet::new();
        for name in &statement.group_by {
            if !seen_group_fields.insert(name.as_str()) {
                return Err(QueryError::DuplicateGroupField { name: name.clone() });
            }
            group_fields.push(field_index(table, name)?);
        }

        let mut fields = Vec::new();
        let mut projections = Vec::new();
        let mut aggregates = Vec::new();
        let mut output_names = HashSet::new();
        for item in items {
            let (projection, field) =
                plan_projection(table, item, grouped, &group_fields, &mut aggregates)?;
            if !output_names.insert(field.name().to_owned()) {
                return Err(QueryError::DuplicateOutputField {
                    name: field.name().to_owned(),
                });
            }
            projections.push(projection);
            fields.push(field);
        }

        let order_by = plan_order_by(&fields, &statement.order_by)?;
        Ok(Self {
            source_fields: table.fields().to_vec(),
            fields,
            projections,
            group_fields,
            aggregates,
            order_by,
            limit: statement.limit,
            grouped,
        })
    }

    /// Returns the planned output schema in projection order.
    #[must_use]
    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    /// Executes the plan over all rows or a same-length filtered selection.
    pub fn execute(
        &self,
        table: &Table,
        selection: Option<&RowSelection>,
    ) -> Result<Table, QueryError> {
        if table.fields() != self.source_fields {
            return Err(QueryError::SourceSchemaMismatch);
        }
        if let Some(selection) = selection
            && selection.len() != table.len()
        {
            return Err(QueryError::SelectionLengthMismatch {
                table_rows: table.len(),
                selection_rows: selection.len(),
            });
        }

        let rows: Box<dyn Iterator<Item = usize> + '_> = match selection {
            Some(selection) => Box::new(selection.selected_rows()),
            None => Box::new(0..table.len()),
        };
        let mut result_rows = if self.grouped {
            self.execute_grouped(table, rows)?
        } else {
            rows.map(|row| self.project_row(table, row)).collect()
        };

        if !self.order_by.is_empty() {
            result_rows.sort_by(|left, right| compare_rows(left, right, &self.order_by));
        }
        if let Some(limit) = self.limit {
            result_rows.truncate(limit);
        }

        let mut result = Table::with_row_limit(self.fields.clone(), result_rows.len())
            .map_err(QueryError::ResultConstruction)?;
        result
            .insert_batch(result_rows)
            .map_err(QueryError::ResultConstruction)?;
        Ok(result)
    }

    fn project_row(&self, table: &Table, row: usize) -> Vec<Value> {
        self.projections
            .iter()
            .map(|projection| match projection {
                PlannedProjection::Column { source, .. } => table.value_at(*source, row),
                PlannedProjection::Aggregate { .. } => {
                    unreachable!("aggregate projections use grouped execution")
                }
            })
            .collect()
    }

    fn execute_grouped(
        &self,
        table: &Table,
        rows: impl Iterator<Item = usize>,
    ) -> Result<Vec<Vec<Value>>, QueryError> {
        let mut groups = Vec::new();
        let mut indexes = HashMap::new();
        if self.group_fields.is_empty() {
            indexes.insert(GroupKey(Vec::new()), 0);
            groups.push(GroupState::new(Vec::new(), &self.aggregates));
        }

        for row in rows {
            let values: Vec<_> = self
                .group_fields
                .iter()
                .map(|column| table.value_at(*column, row))
                .collect();
            let key = GroupKey(values.iter().map(ValueKey::from).collect());
            let group = match indexes.get(&key).copied() {
                Some(group) => group,
                None => {
                    let group = groups.len();
                    indexes.insert(key, group);
                    groups.push(GroupState::new(values, &self.aggregates));
                    group
                }
            };
            groups[group].update(table, row, &self.aggregates)?;
        }

        Ok(groups
            .into_iter()
            .map(|state| {
                self.projections
                    .iter()
                    .map(|projection| match projection {
                        PlannedProjection::Column {
                            group: Some(position),
                            ..
                        } => group_value(&state, *position),
                        PlannedProjection::Aggregate { accumulator } => {
                            state.accumulators[*accumulator].finish()
                        }
                        PlannedProjection::Column { group: None, .. } => {
                            unreachable!("grouped columns have a group position")
                        }
                    })
                    .collect()
            })
            .collect())
    }
}

fn group_value(group: &GroupState, position: usize) -> Value {
    group.values[position].clone()
}

#[derive(Clone, Debug)]
enum PlannedProjection {
    Column { source: usize, group: Option<usize> },
    Aggregate { accumulator: usize },
}

#[derive(Clone, Debug)]
struct AggregateSpec {
    function: AggregateFunction,
    source: Option<usize>,
    field: String,
    data_type: DataType,
}

#[derive(Clone, Copy, Debug)]
struct PlannedOrder {
    field: usize,
    descending: bool,
}

fn projection_items(table: &Table, projection: &SelectProjection) -> Vec<SelectItem> {
    match projection {
        SelectProjection::All => table
            .fields()
            .iter()
            .map(|field| SelectItem {
                expression: SelectExpression::Column(field.name().to_owned()),
                alias: None,
            })
            .collect(),
        SelectProjection::Columns(names) => names
            .iter()
            .map(|name| SelectItem {
                expression: SelectExpression::Column(name.clone()),
                alias: None,
            })
            .collect(),
        SelectProjection::Expressions(items) => items.clone(),
    }
}

fn plan_projection(
    table: &Table,
    item: SelectItem,
    grouped: bool,
    group_fields: &[usize],
    aggregates: &mut Vec<AggregateSpec>,
) -> Result<(PlannedProjection, Field), QueryError> {
    match item.expression {
        SelectExpression::Column(name) => {
            let source = field_index(table, &name)?;
            let group = if grouped {
                Some(
                    group_fields
                        .iter()
                        .position(|field| *field == source)
                        .ok_or_else(|| QueryError::UngroupedColumn { name: name.clone() })?,
                )
            } else {
                None
            };
            let output_name = item.alias.unwrap_or(name);
            Ok((
                PlannedProjection::Column { source, group },
                Field::new(output_name, table.fields()[source].data_type()),
            ))
        }
        SelectExpression::Aggregate { function, argument } => {
            let source = argument
                .as_deref()
                .map(|name| field_index(table, name))
                .transpose()?;
            let source_type = source.map(|column| table.fields()[column].data_type());
            if matches!(function, AggregateFunction::Sum | AggregateFunction::Avg)
                && !matches!(source_type, Some(DataType::Int64 | DataType::Float64))
            {
                let field = argument.unwrap_or_else(|| "*".to_owned());
                return Err(QueryError::NonNumericAggregate {
                    function,
                    field,
                    data_type: source_type.unwrap_or(DataType::Int64),
                });
            }
            let data_type = match function {
                AggregateFunction::Count => DataType::Int64,
                AggregateFunction::Avg => DataType::Float64,
                AggregateFunction::Sum | AggregateFunction::Min | AggregateFunction::Max => {
                    source_type.expect("these aggregate functions require a parsed field")
                }
            };
            let expression_name = match &argument {
                Some(argument) => format!("{}({argument})", function.as_str()),
                None => format!("{}()", function.as_str()),
            };
            let accumulator = aggregates.len();
            aggregates.push(AggregateSpec {
                function,
                source,
                field: argument.unwrap_or_else(|| "*".to_owned()),
                data_type,
            });
            Ok((
                PlannedProjection::Aggregate { accumulator },
                Field::new(item.alias.unwrap_or(expression_name), data_type),
            ))
        }
    }
}

fn plan_order_by(
    fields: &[Field],
    order_by: &[OrderByExpression],
) -> Result<Vec<PlannedOrder>, QueryError> {
    order_by
        .iter()
        .map(|order| {
            let field = fields
                .iter()
                .position(|field| field.name() == order.field)
                .ok_or_else(|| QueryError::OrderFieldNotFound {
                    name: order.field.clone(),
                })?;
            Ok(PlannedOrder {
                field,
                descending: order.descending,
            })
        })
        .collect()
}

fn field_index(table: &Table, name: &str) -> Result<usize, QueryError> {
    table
        .fields()
        .iter()
        .position(|field| field.name() == name)
        .ok_or_else(|| QueryError::FieldNotFound {
            name: name.to_owned(),
        })
}

#[derive(Debug)]
struct GroupState {
    values: Vec<Value>,
    accumulators: Vec<Accumulator>,
}

impl GroupState {
    fn new(values: Vec<Value>, aggregates: &[AggregateSpec]) -> Self {
        Self {
            values,
            accumulators: aggregates.iter().map(Accumulator::new).collect(),
        }
    }

    fn update(
        &mut self,
        table: &Table,
        row: usize,
        aggregates: &[AggregateSpec],
    ) -> Result<(), QueryError> {
        for (accumulator, spec) in self.accumulators.iter_mut().zip(aggregates) {
            let value = spec.source.map(|column| table.value_at(column, row));
            accumulator.update(value.as_ref(), spec)?;
        }
        Ok(())
    }
}

#[derive(Debug)]
enum Accumulator {
    Count(i64),
    SumInt(i64),
    SumFloat(f64),
    Min(Option<Value>, DataType),
    Max(Option<Value>, DataType),
    Avg { sum: f64, count: usize },
}

impl Accumulator {
    fn new(spec: &AggregateSpec) -> Self {
        match (spec.function, spec.data_type) {
            (AggregateFunction::Count, _) => Self::Count(0),
            (AggregateFunction::Sum, DataType::Int64) => Self::SumInt(0),
            (AggregateFunction::Sum, DataType::Float64) => Self::SumFloat(0.0),
            (AggregateFunction::Min, data_type) => Self::Min(None, data_type),
            (AggregateFunction::Max, data_type) => Self::Max(None, data_type),
            (AggregateFunction::Avg, _) => Self::Avg { sum: 0.0, count: 0 },
            _ => unreachable!("aggregate types are validated while planning"),
        }
    }

    fn update(&mut self, value: Option<&Value>, spec: &AggregateSpec) -> Result<(), QueryError> {
        match self {
            Self::Count(count) => {
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| QueryError::Int64Overflow {
                        function: spec.function,
                        field: spec.field.clone(),
                    })?;
            }
            Self::SumInt(sum) => {
                let Value::Int64(value) = value.expect("SUM has a source field") else {
                    unreachable!("SUM source type is planned")
                };
                *sum = sum
                    .checked_add(*value)
                    .ok_or_else(|| QueryError::Int64Overflow {
                        function: spec.function,
                        field: spec.field.clone(),
                    })?;
            }
            Self::SumFloat(sum) => {
                let Value::Float64(value) = value.expect("SUM has a source field") else {
                    unreachable!("SUM source type is planned")
                };
                *sum += value;
            }
            Self::Min(current, _) => update_extreme(current, value, Ordering::is_gt),
            Self::Max(current, _) => update_extreme(current, value, Ordering::is_lt),
            Self::Avg { sum, count } => {
                match value.expect("AVG has a source field") {
                    Value::Int64(value) => *sum += *value as f64,
                    Value::Float64(value) => *sum += value,
                    _ => unreachable!("AVG source type is planned"),
                }
                *count += 1;
            }
        }
        Ok(())
    }

    fn finish(&self) -> Value {
        match self {
            Self::Count(value) | Self::SumInt(value) => Value::Int64(*value),
            Self::SumFloat(value) => Value::Float64(*value),
            Self::Min(value, data_type) | Self::Max(value, data_type) => {
                value.clone().unwrap_or_else(|| default_value(*data_type))
            }
            Self::Avg { sum, count } => Value::Float64(if *count == 0 {
                f64::NAN
            } else {
                *sum / *count as f64
            }),
        }
    }
}

fn update_extreme(
    current: &mut Option<Value>,
    candidate: Option<&Value>,
    replace: impl Fn(Ordering) -> bool,
) {
    let candidate = candidate.expect("MIN and MAX have source fields");
    if current
        .as_ref()
        .is_none_or(|value| replace(compare_values(value, candidate)))
    {
        *current = Some(candidate.clone());
    }
}

fn default_value(data_type: DataType) -> Value {
    match data_type {
        DataType::Int64 => Value::Int64(0),
        DataType::Float64 => Value::Float64(0.0),
        DataType::Bool => Value::Bool(false),
        DataType::String => Value::String(String::new()),
    }
}

fn compare_rows(left: &[Value], right: &[Value], order_by: &[PlannedOrder]) -> Ordering {
    for order in order_by {
        let ordering = compare_values(&left[order.field], &right[order.field]);
        let ordering = if order.descending {
            ordering.reverse()
        } else {
            ordering
        };
        if !ordering.is_eq() {
            return ordering;
        }
    }
    Ordering::Equal
}

fn compare_values(left: &Value, right: &Value) -> Ordering {
    match (left, right) {
        (Value::Int64(left), Value::Int64(right)) => left.cmp(right),
        (Value::Float64(left), Value::Float64(right)) => left.total_cmp(right),
        (Value::Bool(left), Value::Bool(right)) => left.cmp(right),
        (Value::String(left), Value::String(right)) => left.cmp(right),
        _ => unreachable!("query result columns have one planned type"),
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct GroupKey(Vec<ValueKey>);

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum ValueKey {
    Int64(i64),
    Float64(u64),
    Bool(bool),
    String(String),
}

impl From<&Value> for ValueKey {
    fn from(value: &Value) -> Self {
        match value {
            Value::Int64(value) => Self::Int64(*value),
            Value::Float64(value) => Self::Float64(if *value == 0.0 {
                0.0_f64.to_bits()
            } else if value.is_nan() {
                f64::NAN.to_bits()
            } else {
                value.to_bits()
            }),
            Value::Bool(value) => Self::Bool(*value),
            Value::String(value) => Self::String(value.clone()),
        }
    }
}
