//! Aggregate planning and execution kernels.

use crate::error::{Error, Result};
use crate::sql::AggregateFunction;
use crate::storage::{Column, Table};
use crate::value::{DataType, Value};

const GLOBAL_BLOCK_SIZE: usize = 1_024;

#[derive(Debug, Clone)]
pub(crate) struct AggregateSpec {
    function: AggregateFunction,
    argument: Option<usize>,
    input_type: Option<DataType>,
}

impl AggregateSpec {
    pub(crate) fn new(
        function: AggregateFunction,
        argument: Option<usize>,
        input_type: Option<DataType>,
    ) -> Result<Self> {
        if matches!(function, AggregateFunction::Sum | AggregateFunction::Avg)
            && !matches!(input_type, Some(DataType::Int64 | DataType::Float64))
        {
            let actual = input_type.map_or_else(|| "*".to_owned(), |value| value.to_string());
            return Err(Error::TypeMismatch {
                context: format!("{} argument", function.name()),
                expected: "Int64 or Float64".to_owned(),
                actual,
            });
        }

        Ok(Self {
            function,
            argument,
            input_type,
        })
    }

    pub(crate) fn output_type(&self) -> DataType {
        match self.function {
            AggregateFunction::Count => DataType::Int64,
            AggregateFunction::Avg => DataType::Float64,
            AggregateFunction::Sum | AggregateFunction::Min | AggregateFunction::Max => {
                self.input_type.expect("validated column argument")
            }
        }
    }
}

/// Executes a predicate-free global aggregate scan using typed column blocks.
pub(crate) fn execute_global(table: &Table, specs: &[AggregateSpec]) -> Result<Vec<Value>> {
    execute_global_with_block_size(table, specs, GLOBAL_BLOCK_SIZE)
}

fn execute_global_with_block_size(
    table: &Table,
    specs: &[AggregateSpec],
    block_size: usize,
) -> Result<Vec<Value>> {
    assert!(block_size > 0, "aggregate block size must be non-zero");

    let count = i64::try_from(table.row_count()).ok();
    let mut outputs = std::iter::repeat_with(|| None)
        .take(specs.len())
        .collect::<Vec<Option<Result<Value>>>>();
    let mut overflows = Vec::new();
    let plans = build_source_plans(specs, &mut outputs, count, &mut overflows);

    for plan in &plans {
        match &table.columns()[plan.source] {
            Column::Int64(values) => {
                execute_int64(values, plan, block_size, &mut outputs, &mut overflows);
            }
            Column::Float64(values) => {
                execute_float64(values, plan, block_size, &mut outputs, &mut overflows);
            }
            Column::Bool(values) => execute_bool(values, plan, block_size, &mut outputs),
            Column::String(values) => execute_string(values, plan, block_size, &mut outputs),
        }
    }

    if let Some(overflow) = overflows
        .into_iter()
        .min_by_key(|overflow| (overflow.row, overflow.output))
    {
        return Err(Error::NumericOverflow(overflow.operation.to_owned()));
    }

    outputs
        .into_iter()
        .map(|output| output.expect("every aggregate has an output"))
        .collect()
}

#[derive(Debug, Default)]
struct Operations {
    sum: bool,
    min: bool,
    max: bool,
    avg: bool,
}

impl Operations {
    fn include(&mut self, function: AggregateFunction) {
        match function {
            AggregateFunction::Count => unreachable!("COUNT does not scan a source column"),
            AggregateFunction::Sum => self.sum = true,
            AggregateFunction::Min => self.min = true,
            AggregateFunction::Max => self.max = true,
            AggregateFunction::Avg => self.avg = true,
        }
    }
}

#[derive(Debug)]
struct RequestedOperation {
    output: usize,
    function: AggregateFunction,
}

#[derive(Debug)]
struct SourcePlan {
    source: usize,
    operations: Operations,
    requested: Vec<RequestedOperation>,
}

#[derive(Debug)]
struct OverflowAt {
    row: usize,
    output: usize,
    operation: &'static str,
}

fn build_source_plans(
    specs: &[AggregateSpec],
    outputs: &mut [Option<Result<Value>>],
    count: Option<i64>,
    overflows: &mut Vec<OverflowAt>,
) -> Vec<SourcePlan> {
    let mut plans = Vec::<SourcePlan>::new();
    for (output, spec) in specs.iter().enumerate() {
        if spec.function == AggregateFunction::Count {
            if let Some(count) = count {
                outputs[output] = Some(Ok(Value::Int64(count)));
            } else {
                outputs[output] = Some(Err(Error::NumericOverflow("COUNT".to_owned())));
                overflows.push(OverflowAt {
                    row: usize::try_from(i64::MAX).unwrap_or(usize::MAX),
                    output,
                    operation: "COUNT",
                });
            }
            continue;
        }

        let source = spec.argument.expect("validated aggregate argument");
        let plan = if let Some(position) = plans.iter().position(|plan| plan.source == source) {
            &mut plans[position]
        } else {
            plans.push(SourcePlan {
                source,
                operations: Operations::default(),
                requested: Vec::new(),
            });
            plans.last_mut().expect("source plan was inserted")
        };
        plan.operations.include(spec.function);
        plan.requested.push(RequestedOperation {
            output,
            function: spec.function,
        });
    }
    plans
}

fn record_overflow(
    plan: &SourcePlan,
    row: usize,
    function: AggregateFunction,
    operation: &'static str,
    overflows: &mut Vec<OverflowAt>,
) {
    if let Some(requested) = plan
        .requested
        .iter()
        .find(|requested| requested.function == function)
    {
        overflows.push(OverflowAt {
            row,
            output: requested.output,
            operation,
        });
    }
}

fn execute_int64(
    values: &[i64],
    plan: &SourcePlan,
    block_size: usize,
    outputs: &mut [Option<Result<Value>>],
    overflows: &mut Vec<OverflowAt>,
) {
    let mut sum = 0_i64;
    let mut sum_overflow = false;
    let mut avg_sum = 0_i128;
    let mut avg_count = 0_u64;
    let mut avg_overflow = None;
    let mut min = None;
    let mut max = None;
    let mut row = 0;

    for block in values.chunks(block_size) {
        for &value in block {
            if plan.operations.sum && !sum_overflow {
                if let Some(next) = sum.checked_add(value) {
                    sum = next;
                } else {
                    sum_overflow = true;
                    record_overflow(plan, row, AggregateFunction::Sum, "SUM(Int64)", overflows);
                }
            }
            if plan.operations.avg && avg_overflow.is_none() {
                if let Some(next) = avg_sum.checked_add(i128::from(value)) {
                    avg_sum = next;
                    if let Some(next) = avg_count.checked_add(1) {
                        avg_count = next;
                    } else {
                        avg_overflow = Some("AVG count");
                    }
                } else {
                    avg_overflow = Some("AVG(Int64) sum");
                }
                if let Some(operation) = avg_overflow {
                    record_overflow(plan, row, AggregateFunction::Avg, operation, overflows);
                }
            }
            update_ordered_extrema(
                value,
                plan.operations.min,
                plan.operations.max,
                &mut min,
                &mut max,
            );
            row += 1;
        }
    }

    for requested in &plan.requested {
        let output = match requested.function {
            AggregateFunction::Sum if sum_overflow => {
                Err(Error::NumericOverflow("SUM(Int64)".to_owned()))
            }
            AggregateFunction::Sum => Ok(Value::Int64(sum)),
            AggregateFunction::Min => min.map(Value::Int64).ok_or_else(empty_min_error),
            AggregateFunction::Max => max.map(Value::Int64).ok_or_else(empty_max_error),
            AggregateFunction::Avg => match avg_overflow {
                Some(operation) => Err(Error::NumericOverflow(operation.to_owned())),
                None if avg_count > 0 => Ok(Value::Float64(avg_sum as f64 / avg_count as f64)),
                None => Err(empty_avg_error()),
            },
            AggregateFunction::Count => unreachable!("COUNT is filled without a column scan"),
        };
        outputs[requested.output] = Some(output);
    }
}

fn execute_float64(
    values: &[f64],
    plan: &SourcePlan,
    block_size: usize,
    outputs: &mut [Option<Result<Value>>],
    overflows: &mut Vec<OverflowAt>,
) {
    let needs_sum = plan.operations.sum || plan.operations.avg;
    let mut sum = 0.0_f64;
    let mut count = 0_u64;
    let mut sum_overflow = false;
    let mut count_overflow = false;
    let mut min = None;
    let mut max = None;
    let mut row = 0;

    for block in values.chunks(block_size) {
        for &value in block {
            if needs_sum && !sum_overflow {
                sum += value;
                if !sum.is_finite() {
                    sum_overflow = true;
                    record_overflow(plan, row, AggregateFunction::Sum, "SUM(Float64)", overflows);
                    record_overflow(
                        plan,
                        row,
                        AggregateFunction::Avg,
                        "AVG(Float64) sum",
                        overflows,
                    );
                }
            }
            if plan.operations.avg && !sum_overflow && !count_overflow {
                if let Some(next) = count.checked_add(1) {
                    count = next;
                } else {
                    count_overflow = true;
                    record_overflow(plan, row, AggregateFunction::Avg, "AVG count", overflows);
                }
            }
            update_float_extrema(
                value,
                plan.operations.min,
                plan.operations.max,
                &mut min,
                &mut max,
            );
            row += 1;
        }
    }

    for requested in &plan.requested {
        let output = match requested.function {
            AggregateFunction::Sum if sum_overflow => {
                Err(Error::NumericOverflow("SUM(Float64)".to_owned()))
            }
            AggregateFunction::Sum => Ok(Value::Float64(sum)),
            AggregateFunction::Min => min.map(Value::Float64).ok_or_else(empty_min_error),
            AggregateFunction::Max => max.map(Value::Float64).ok_or_else(empty_max_error),
            AggregateFunction::Avg if sum_overflow => {
                Err(Error::NumericOverflow("AVG(Float64) sum".to_owned()))
            }
            AggregateFunction::Avg if count_overflow => {
                Err(Error::NumericOverflow("AVG count".to_owned()))
            }
            AggregateFunction::Avg if count > 0 => Ok(Value::Float64(sum / count as f64)),
            AggregateFunction::Avg => Err(empty_avg_error()),
            AggregateFunction::Count => unreachable!("COUNT is filled without a column scan"),
        };
        outputs[requested.output] = Some(output);
    }
}

fn execute_bool(
    values: &[bool],
    plan: &SourcePlan,
    block_size: usize,
    outputs: &mut [Option<Result<Value>>],
) {
    let mut min = None;
    let mut max = None;
    for block in values.chunks(block_size) {
        for &value in block {
            update_ordered_extrema(
                value,
                plan.operations.min,
                plan.operations.max,
                &mut min,
                &mut max,
            );
        }
    }

    for requested in &plan.requested {
        let output = match requested.function {
            AggregateFunction::Min => min.map(Value::Bool).ok_or_else(empty_min_error),
            AggregateFunction::Max => max.map(Value::Bool).ok_or_else(empty_max_error),
            _ => unreachable!("Boolean aggregate was validated"),
        };
        outputs[requested.output] = Some(output);
    }
}

fn execute_string(
    values: &[String],
    plan: &SourcePlan,
    block_size: usize,
    outputs: &mut [Option<Result<Value>>],
) {
    let mut min = None;
    let mut max = None;
    for block in values.chunks(block_size) {
        for value in block {
            update_ordered_extrema(
                value.as_str(),
                plan.operations.min,
                plan.operations.max,
                &mut min,
                &mut max,
            );
        }
    }

    for requested in &plan.requested {
        let output = match requested.function {
            AggregateFunction::Min => min
                .map(|value| Value::String(value.to_owned()))
                .ok_or_else(empty_min_error),
            AggregateFunction::Max => max
                .map(|value| Value::String(value.to_owned()))
                .ok_or_else(empty_max_error),
            _ => unreachable!("String aggregate was validated"),
        };
        outputs[requested.output] = Some(output);
    }
}

fn update_ordered_extrema<T: Copy + Ord>(
    value: T,
    update_min: bool,
    update_max: bool,
    min: &mut Option<T>,
    max: &mut Option<T>,
) {
    if update_min && min.is_none_or(|current| value < current) {
        *min = Some(value);
    }
    if update_max && max.is_none_or(|current| value > current) {
        *max = Some(value);
    }
}

fn update_float_extrema(
    value: f64,
    update_min: bool,
    update_max: bool,
    min: &mut Option<f64>,
    max: &mut Option<f64>,
) {
    if update_min && min.is_none_or(|current| float_less(value, current)) {
        *min = Some(value);
    }
    if update_max && max.is_none_or(|current| float_less(current, value)) {
        *max = Some(value);
    }
}

fn float_less(left: f64, right: f64) -> bool {
    left != right && left.total_cmp(&right).is_lt()
}

fn empty_min_error() -> Error {
    Error::InvalidQuery("MIN is undefined for an empty input".to_owned())
}

fn empty_max_error() -> Error {
    Error::InvalidQuery("MAX is undefined for an empty input".to_owned())
}

fn empty_avg_error() -> Error {
    Error::InvalidQuery("AVG is undefined for an empty input".to_owned())
}

#[derive(Debug)]
pub(crate) enum AggregateState {
    Count(i64),
    SumInt(i64),
    SumFloat(f64),
    Min(Option<Value>),
    Max(Option<Value>),
    AvgInt { sum: i128, count: u64 },
    AvgFloat { sum: f64, count: u64 },
}

impl AggregateState {
    pub(crate) fn new(spec: &AggregateSpec) -> Self {
        match spec.function {
            AggregateFunction::Count => Self::Count(0),
            AggregateFunction::Sum if spec.input_type == Some(DataType::Int64) => Self::SumInt(0),
            AggregateFunction::Sum => Self::SumFloat(0.0),
            AggregateFunction::Min => Self::Min(None),
            AggregateFunction::Max => Self::Max(None),
            AggregateFunction::Avg if spec.input_type == Some(DataType::Int64) => {
                Self::AvgInt { sum: 0, count: 0 }
            }
            AggregateFunction::Avg => Self::AvgFloat { sum: 0.0, count: 0 },
        }
    }

    pub(crate) fn update(&mut self, spec: &AggregateSpec, table: &Table, row: usize) -> Result<()> {
        match self {
            Self::Count(count) => {
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| Error::NumericOverflow("COUNT".to_owned()))?;
            }
            Self::SumInt(sum) => {
                let Column::Int64(values) = &table.columns()[spec.argument.expect("SUM argument")]
                else {
                    unreachable!("SUM input type is resolved")
                };
                *sum = sum
                    .checked_add(values[row])
                    .ok_or_else(|| Error::NumericOverflow("SUM(Int64)".to_owned()))?;
            }
            Self::SumFloat(sum) => {
                let Column::Float64(values) =
                    &table.columns()[spec.argument.expect("SUM argument")]
                else {
                    unreachable!("SUM input type is resolved")
                };
                *sum += values[row];
                if !sum.is_finite() {
                    return Err(Error::NumericOverflow("SUM(Float64)".to_owned()));
                }
            }
            Self::Min(current) => {
                let column = &table.columns()[spec.argument.expect("MIN argument")];
                let candidate = column.value_ref(row);
                if current
                    .as_ref()
                    .is_none_or(|existing| candidate < existing.as_ref())
                {
                    *current = Some(candidate.to_owned());
                }
            }
            Self::Max(current) => {
                let column = &table.columns()[spec.argument.expect("MAX argument")];
                let candidate = column.value_ref(row);
                if current
                    .as_ref()
                    .is_none_or(|existing| candidate > existing.as_ref())
                {
                    *current = Some(candidate.to_owned());
                }
            }
            Self::AvgInt { sum, count } => {
                let Column::Int64(values) = &table.columns()[spec.argument.expect("AVG argument")]
                else {
                    unreachable!("AVG input type is resolved")
                };
                *sum = sum
                    .checked_add(i128::from(values[row]))
                    .ok_or_else(|| Error::NumericOverflow("AVG(Int64) sum".to_owned()))?;
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| Error::NumericOverflow("AVG count".to_owned()))?;
            }
            Self::AvgFloat { sum, count } => {
                let Column::Float64(values) =
                    &table.columns()[spec.argument.expect("AVG argument")]
                else {
                    unreachable!("AVG input type is resolved")
                };
                *sum += values[row];
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| Error::NumericOverflow("AVG count".to_owned()))?;
                if !sum.is_finite() {
                    return Err(Error::NumericOverflow("AVG(Float64) sum".to_owned()));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<Value> {
        match self {
            Self::Count(value) | Self::SumInt(value) => Ok(Value::Int64(value)),
            Self::SumFloat(value) => Ok(Value::Float64(value)),
            Self::Min(Some(value)) | Self::Max(Some(value)) => Ok(value),
            Self::AvgInt { sum, count } if count > 0 => {
                Ok(Value::Float64(sum as f64 / count as f64))
            }
            Self::AvgFloat { sum, count } if count > 0 => Ok(Value::Float64(sum / count as f64)),
            Self::Min(None) => Err(empty_min_error()),
            Self::Max(None) => Err(empty_max_error()),
            Self::AvgInt { .. } | Self::AvgFloat { .. } => Err(empty_avg_error()),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::storage::ColumnDef;

    use super::*;

    fn spec(
        function: AggregateFunction,
        argument: Option<usize>,
        input_type: Option<DataType>,
    ) -> AggregateSpec {
        AggregateSpec::new(function, argument, input_type).expect("valid aggregate")
    }

    fn mixed_table() -> Table {
        let mut table = Table::new(
            "metrics".to_owned(),
            vec![
                ColumnDef {
                    name: "integer".to_owned(),
                    data_type: DataType::Int64,
                },
                ColumnDef {
                    name: "float".to_owned(),
                    data_type: DataType::Float64,
                },
                ColumnDef {
                    name: "flag".to_owned(),
                    data_type: DataType::Bool,
                },
                ColumnDef {
                    name: "label".to_owned(),
                    data_type: DataType::String,
                },
            ],
        )
        .expect("valid table");
        for row in [
            vec![
                Value::Int64(9_007_199_254_740_993),
                Value::Float64(1.25),
                Value::Bool(true),
                Value::String("beta".to_owned()),
            ],
            vec![
                Value::Int64(1),
                Value::Float64(-4.5),
                Value::Bool(false),
                Value::String("alpha".to_owned()),
            ],
            vec![
                Value::Int64(-9_007_199_254_740_993),
                Value::Float64(8.0),
                Value::Bool(true),
                Value::String("gamma".to_owned()),
            ],
        ] {
            table.insert_row(row).expect("valid row");
        }
        table
    }

    #[test]
    fn mixed_global_aggregates_are_deterministic_across_block_sizes() {
        let table = mixed_table();
        let specs = vec![
            spec(AggregateFunction::Count, None, None),
            spec(AggregateFunction::Max, Some(3), Some(DataType::String)),
            spec(AggregateFunction::Sum, Some(0), Some(DataType::Int64)),
            spec(AggregateFunction::Min, Some(0), Some(DataType::Int64)),
            spec(AggregateFunction::Max, Some(0), Some(DataType::Int64)),
            spec(AggregateFunction::Avg, Some(0), Some(DataType::Int64)),
            spec(AggregateFunction::Sum, Some(1), Some(DataType::Float64)),
            spec(AggregateFunction::Avg, Some(1), Some(DataType::Float64)),
            spec(AggregateFunction::Min, Some(2), Some(DataType::Bool)),
            spec(AggregateFunction::Max, Some(2), Some(DataType::Bool)),
            spec(AggregateFunction::Min, Some(3), Some(DataType::String)),
            spec(AggregateFunction::Count, Some(1), Some(DataType::Float64)),
            spec(AggregateFunction::Sum, Some(0), Some(DataType::Int64)),
        ];
        let expected = vec![
            Value::Int64(3),
            Value::String("gamma".to_owned()),
            Value::Int64(1),
            Value::Int64(-9_007_199_254_740_993),
            Value::Int64(9_007_199_254_740_993),
            Value::Float64(1.0 / 3.0),
            Value::Float64(4.75),
            Value::Float64(4.75 / 3.0),
            Value::Bool(false),
            Value::Bool(true),
            Value::String("alpha".to_owned()),
            Value::Int64(3),
            Value::Int64(1),
        ];

        for block_size in [1, 2, 3, 4, 64, 1_024] {
            assert_eq!(
                execute_global_with_block_size(&table, &specs, block_size)
                    .expect("aggregate succeeds"),
                expected,
                "block size {block_size}"
            );
        }

        let mut slots = std::iter::repeat_with(|| None)
            .take(specs.len())
            .collect::<Vec<_>>();
        let mut overflows = Vec::new();
        let plans = build_source_plans(&specs, &mut slots, Some(3), &mut overflows);
        assert_eq!(plans.len(), 4);
        assert_eq!(
            plans
                .iter()
                .find(|plan| plan.source == 0)
                .unwrap()
                .requested
                .len(),
            5
        );
    }

    #[test]
    fn global_kernels_retain_checked_numeric_overflow() {
        let mut integers = Table::new(
            "integers".to_owned(),
            vec![ColumnDef {
                name: "value".to_owned(),
                data_type: DataType::Int64,
            }],
        )
        .expect("valid table");
        integers.insert_row(vec![Value::Int64(i64::MAX)]).unwrap();
        integers.insert_row(vec![Value::Int64(1)]).unwrap();
        assert_eq!(
            execute_global(
                &integers,
                &[spec(AggregateFunction::Sum, Some(0), Some(DataType::Int64))]
            ),
            Err(Error::NumericOverflow("SUM(Int64)".to_owned()))
        );

        let mut floats = Table::new(
            "floats".to_owned(),
            vec![ColumnDef {
                name: "value".to_owned(),
                data_type: DataType::Float64,
            }],
        )
        .expect("valid table");
        floats.insert_row(vec![Value::Float64(f64::MAX)]).unwrap();
        floats.insert_row(vec![Value::Float64(f64::MAX)]).unwrap();
        assert_eq!(
            execute_global(
                &floats,
                &[spec(
                    AggregateFunction::Avg,
                    Some(0),
                    Some(DataType::Float64)
                )]
            ),
            Err(Error::NumericOverflow("AVG(Float64) sum".to_owned()))
        );
    }

    #[test]
    fn empty_global_inputs_preserve_aggregate_semantics() {
        let table = Table::new(
            "empty".to_owned(),
            vec![ColumnDef {
                name: "value".to_owned(),
                data_type: DataType::Int64,
            }],
        )
        .expect("valid table");
        assert_eq!(
            execute_global(
                &table,
                &[
                    spec(AggregateFunction::Count, None, None),
                    spec(AggregateFunction::Sum, Some(0), Some(DataType::Int64)),
                ]
            )
            .expect("COUNT and SUM are defined"),
            vec![Value::Int64(0), Value::Int64(0)]
        );
        assert_eq!(
            execute_global(
                &table,
                &[spec(AggregateFunction::Min, Some(0), Some(DataType::Int64))]
            ),
            Err(empty_min_error())
        );
        assert_eq!(
            execute_global(
                &table,
                &[spec(AggregateFunction::Max, Some(0), Some(DataType::Int64))]
            ),
            Err(empty_max_error())
        );
        assert_eq!(
            execute_global(
                &table,
                &[spec(AggregateFunction::Avg, Some(0), Some(DataType::Int64))]
            ),
            Err(empty_avg_error())
        );
    }
}
