//! Private partial-state reduction for non-nullable `Int64` `SUM` and `AVG`
//! grouped by `Bool`.

use super::aggregate_scheduler::{GlobalAggregateParallelism, run_grouped_aggregate};
use super::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Operation {
    Sum,
    Avg,
}

impl Operation {
    const fn worker_name_prefix(self) -> &'static str {
        match self {
            Self::Sum => "rusthouse-group-bool-sum-int64",
            Self::Avg => "rusthouse-group-bool-avg-int64",
        }
    }

    const fn sum_overflow_context(self) -> &'static str {
        match self {
            Self::Sum => "SUM(Int64) exact sum",
            Self::Avg => "AVG(Int64) sum",
        }
    }

    const fn count_overflow_context(self) -> &'static str {
        match self {
            Self::Sum => "SUM count",
            Self::Avg => "AVG count",
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct GroupPartial {
    sum: i128,
    count: u64,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Partial {
    false_group: GroupPartial,
    true_group: GroupPartial,
    first_seen: Option<bool>,
}

impl Partial {
    fn group(&self, value: bool) -> &GroupPartial {
        if value {
            &self.true_group
        } else {
            &self.false_group
        }
    }

    fn group_mut(&mut self, value: bool) -> &mut GroupPartial {
        if value {
            &mut self.true_group
        } else {
            &mut self.false_group
        }
    }

    pub(super) fn first_seen(&self) -> Option<bool> {
        self.first_seen
    }

    pub(super) fn present(&self, value: bool) -> bool {
        self.group(value).count > 0
    }

    pub(super) fn sum_and_count(&self, value: bool) -> (i128, u64) {
        let partial = self.group(value);
        (partial.sum, partial.count)
    }

    fn observe(&mut self, group: bool, value: i64, operation: Operation) -> Result<()> {
        self.first_seen.get_or_insert(group);
        let partial = self.group_mut(group);
        partial.sum = partial
            .sum
            .checked_add(i128::from(value))
            .ok_or_else(|| Error::NumericOverflow(operation.sum_overflow_context().to_owned()))?;
        partial.count = partial
            .count
            .checked_add(1)
            .ok_or_else(|| Error::NumericOverflow(operation.count_overflow_context().to_owned()))?;
        Ok(())
    }
}

pub(super) fn reduce(
    group_values: &[bool],
    sum_values: &[i64],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
    operation: Operation,
) -> Result<Partial> {
    reduce_with_chunk(
        group_values,
        sum_values,
        matching_rows,
        parallelism,
        operation,
        scan_chunk,
    )
}

fn reduce_with_chunk<C>(
    group_values: &[bool],
    sum_values: &[i64],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
    operation: Operation,
    chunk: C,
) -> Result<Partial>
where
    C: Fn(&[bool], &[i64], &[usize], Operation) -> Result<Partial> + Sync,
{
    run_grouped_aggregate(
        matching_rows,
        parallelism,
        operation.worker_name_prefix(),
        |rows| chunk(group_values, sum_values, rows, operation),
        |partials| reduce_ordered(partials, operation),
    )
}

fn scan_chunk(
    group_values: &[bool],
    sum_values: &[i64],
    matching_rows: &[usize],
    operation: Operation,
) -> Result<Partial> {
    let mut partial = Partial::default();
    for row in matching_rows {
        partial.observe(group_values[*row], sum_values[*row], operation)?;
    }
    Ok(partial)
}

fn reduce_ordered(partials: Vec<Partial>, operation: Operation) -> Result<Partial> {
    partials
        .into_iter()
        .try_fold(Partial::default(), |mut total, partial| {
            if total.first_seen.is_none() {
                total.first_seen = partial.first_seen;
            }
            total.false_group.sum = total
                .false_group
                .sum
                .checked_add(partial.false_group.sum)
                .ok_or_else(|| {
                    Error::NumericOverflow(operation.sum_overflow_context().to_owned())
                })?;
            total.false_group.count = total
                .false_group
                .count
                .checked_add(partial.false_group.count)
                .ok_or_else(|| {
                    Error::NumericOverflow(operation.count_overflow_context().to_owned())
                })?;
            total.true_group.sum = total
                .true_group
                .sum
                .checked_add(partial.true_group.sum)
                .ok_or_else(|| {
                    Error::NumericOverflow(operation.sum_overflow_context().to_owned())
                })?;
            total.true_group.count = total
                .true_group
                .count
                .checked_add(partial.true_group.count)
                .ok_or_else(|| {
                    Error::NumericOverflow(operation.count_overflow_context().to_owned())
                })?;
            Ok(total)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::aggregate_scheduler::{
        GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD, parallel_aggregate_partition,
    };

    #[test]
    fn worker_failure_repeats_complete_input_and_preserves_ordered_state() {
        let group_values = [true, false, true];
        let sum_values = [1, -5, 9];
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut matching_rows = vec![0; row_count];
        let second_partition = parallel_aggregate_partition(&matching_rows, 2, 0).len();
        matching_rows[second_partition] = 1;
        matching_rows[row_count - 1] = 2;

        let expected = Partial {
            false_group: GroupPartial { sum: -5, count: 1 },
            true_group: GroupPartial {
                sum: i128::try_from(row_count).unwrap() + 7,
                count: u64::try_from(row_count - 1).unwrap(),
            },
            first_seen: Some(true),
        };
        for (operation, failed_worker) in [
            (Operation::Sum, "rusthouse-group-bool-sum-int64-1"),
            (Operation::Avg, "rusthouse-group-bool-avg-int64-1"),
        ] {
            let successful_parallel = reduce(
                &group_values,
                &sum_values,
                &matching_rows,
                GlobalAggregateParallelism::fixed(2),
                operation,
            )
            .expect("deterministic parallel grouped SUM/AVG succeeds");
            let failed_parallel = reduce_with_chunk(
                &group_values,
                &sum_values,
                &matching_rows,
                GlobalAggregateParallelism::fixed(2),
                operation,
                |group_values, sum_values, rows, operation| {
                    if std::thread::current().name() == Some(failed_worker) {
                        panic!("injected grouped SUM/AVG worker failure");
                    }
                    scan_chunk(group_values, sum_values, rows, operation)
                },
            )
            .expect("worker failure falls back to the complete grouped input locally");

            assert_eq!(successful_parallel, expected);
            assert_eq!(failed_parallel, expected);
        }
    }

    #[test]
    fn ordered_reduction_uses_operation_specific_overflow_contexts() {
        for (operation, expected_sum, expected_count) in [
            (Operation::Sum, "SUM(Int64) exact sum", "SUM count"),
            (Operation::Avg, "AVG(Int64) sum", "AVG count"),
        ] {
            assert_eq!(
                reduce_ordered(
                    vec![
                        Partial {
                            false_group: GroupPartial {
                                sum: i128::MAX,
                                count: 0,
                            },
                            ..Partial::default()
                        },
                        Partial {
                            false_group: GroupPartial { sum: 1, count: 0 },
                            ..Partial::default()
                        },
                    ],
                    operation,
                ),
                Err(Error::NumericOverflow(expected_sum.to_owned()))
            );
            assert_eq!(
                reduce_ordered(
                    vec![
                        Partial {
                            true_group: GroupPartial {
                                sum: 0,
                                count: u64::MAX,
                            },
                            ..Partial::default()
                        },
                        Partial {
                            true_group: GroupPartial { sum: 0, count: 1 },
                            ..Partial::default()
                        },
                    ],
                    operation,
                ),
                Err(Error::NumericOverflow(expected_count.to_owned()))
            );
        }
    }
}
