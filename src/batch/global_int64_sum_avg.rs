//! Raw-slice reduction for supported global `Int64` `SUM` and `AVG`.
//!
//! This module owns the nullable and non-nullable chunk scans, sum-and-count
//! partials, scoped worker orchestration, and ordered checked reduction. The
//! SQL engine retains query-shape eligibility, physical column dispatch, and
//! conversion of the completed partial into a typed aggregate state.

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
            Self::Sum => "rusthouse-sum-int64",
            Self::Avg => "rusthouse-avg-int64",
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
pub(super) struct Partial {
    sum: i128,
    count: u64,
}

impl Partial {
    pub(super) const fn sum_and_count(self) -> (i128, u64) {
        (self.sum, self.count)
    }

    pub(super) const fn count(&self) -> u64 {
        self.count
    }

    fn observe(&mut self, value: i64, operation: Operation) -> Result<()> {
        self.sum = self
            .sum
            .checked_add(i128::from(value))
            .ok_or_else(|| Error::NumericOverflow(operation.sum_overflow_context().to_owned()))?;
        self.count = self
            .count
            .checked_add(1)
            .ok_or_else(|| Error::NumericOverflow(operation.count_overflow_context().to_owned()))?;
        Ok(())
    }
}

pub(super) fn reduce_int64(
    values: &[i64],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
    operation: Operation,
) -> Result<Partial> {
    reduce_with_chunk(
        values,
        matching_rows,
        parallelism,
        operation,
        scan_int64_chunk,
    )
}

pub(super) fn reduce_nullable_int64(
    values: &[Option<i64>],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
    operation: Operation,
) -> Result<Partial> {
    reduce_with_chunk(
        values,
        matching_rows,
        parallelism,
        operation,
        scan_nullable_int64_chunk,
    )
}

fn reduce_with_chunk<T, C>(
    values: &[T],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
    operation: Operation,
    chunk: C,
) -> Result<Partial>
where
    T: Sync,
    C: Fn(&[T], &[usize], Operation) -> Result<Partial> + Sync,
{
    run_grouped_aggregate(
        matching_rows,
        parallelism,
        operation.worker_name_prefix(),
        |rows| chunk(values, rows, operation),
        |partials| reduce_ordered(partials, operation),
    )
}

fn scan_int64_chunk(
    values: &[i64],
    matching_rows: &[usize],
    operation: Operation,
) -> Result<Partial> {
    let mut partial = Partial::default();
    for row in matching_rows {
        partial.observe(values[*row], operation)?;
    }
    Ok(partial)
}

fn scan_nullable_int64_chunk(
    values: &[Option<i64>],
    matching_rows: &[usize],
    operation: Operation,
) -> Result<Partial> {
    let mut partial = Partial::default();
    for row in matching_rows {
        if let Some(value) = values[*row] {
            partial.observe(value, operation)?;
        }
    }
    Ok(partial)
}

fn reduce_ordered(partials: Vec<Partial>, operation: Operation) -> Result<Partial> {
    partials
        .into_iter()
        .try_fold(Partial::default(), |total, partial| {
            Ok(Partial {
                sum: total.sum.checked_add(partial.sum).ok_or_else(|| {
                    Error::NumericOverflow(operation.sum_overflow_context().to_owned())
                })?,
                count: total.count.checked_add(partial.count).ok_or_else(|| {
                    Error::NumericOverflow(operation.count_overflow_context().to_owned())
                })?,
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::aggregate_scheduler::{
        GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD, TestGlobalAggregateWorkerBudget,
        parallel_aggregate_partition,
    };

    #[test]
    fn raw_slice_scans_preserve_selection_and_nullable_present_count() {
        let rows = [3, 1, 3, 0];
        assert_eq!(
            reduce_int64(
                &[7, -4, 99, 2],
                &rows,
                GlobalAggregateParallelism::fixed(1),
                Operation::Sum,
            ),
            Ok(Partial { sum: 7, count: 4 })
        );
        assert_eq!(
            reduce_nullable_int64(
                &[Some(7), None, Some(-4), Some(2)],
                &rows,
                GlobalAggregateParallelism::fixed(1),
                Operation::Avg,
            ),
            Ok(Partial { sum: 11, count: 3 })
        );
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
                            sum: i128::MAX,
                            count: 0,
                        },
                        Partial { sum: 1, count: 0 },
                    ],
                    operation,
                ),
                Err(Error::NumericOverflow(expected_sum.to_owned()))
            );
            assert_eq!(
                reduce_ordered(
                    vec![
                        Partial {
                            sum: 0,
                            count: u64::MAX,
                        },
                        Partial { sum: 0, count: 1 },
                    ],
                    operation,
                ),
                Err(Error::NumericOverflow(expected_count.to_owned()))
            );
        }
    }

    #[test]
    fn worker_failure_releases_admission_and_repeats_complete_input() {
        static BUDGET: TestGlobalAggregateWorkerBudget =
            TestGlobalAggregateWorkerBudget::for_test(1);

        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut matching_rows = vec![0; row_count];
        let second_partition = parallel_aggregate_partition(&matching_rows, 2, 0).len();
        matching_rows[second_partition] = 1;
        matching_rows[row_count - 1] = 2;

        let values = [Some(1), None, Some(17)];
        let partial = reduce_with_chunk(
            &values,
            &matching_rows,
            GlobalAggregateParallelism::budgeted(
                GlobalAggregateParallelism::fixed(2).worker_cap(),
                &BUDGET,
            ),
            Operation::Avg,
            |values, rows, operation| {
                if rows.len() == row_count {
                    assert_eq!(BUDGET.helpers_in_use(), 0);
                } else if std::thread::current().name() == Some("rusthouse-avg-int64-1") {
                    panic!("injected global Int64 AVG worker failure");
                }
                scan_nullable_int64_chunk(values, rows, operation)
            },
        )
        .expect("worker failure falls back to the complete nullable AVG input");

        assert_eq!(
            partial,
            Partial {
                sum: i128::try_from(row_count).unwrap() + 15,
                count: u64::try_from(row_count - 1).unwrap(),
            }
        );
        assert_eq!(BUDGET.helpers_in_use(), 0);

        let values = [1, 17, 1];
        let partial = reduce_with_chunk(
            &values,
            &matching_rows,
            GlobalAggregateParallelism::budgeted(
                GlobalAggregateParallelism::fixed(2).worker_cap(),
                &BUDGET,
            ),
            Operation::Sum,
            |values, rows, operation| {
                if rows.len() == row_count {
                    assert_eq!(BUDGET.helpers_in_use(), 0);
                } else if std::thread::current().name() == Some("rusthouse-sum-int64-1") {
                    panic!("injected global Int64 SUM worker failure");
                }
                scan_int64_chunk(values, rows, operation)
            },
        )
        .expect("worker failure falls back to the complete non-nullable SUM input");

        assert_eq!(
            partial,
            Partial {
                sum: i128::try_from(row_count).unwrap() + 16,
                count: u64::try_from(row_count).unwrap(),
            }
        );
        assert_eq!(BUDGET.helpers_in_use(), 0);
    }
}
