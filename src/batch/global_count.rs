//! Raw-slice reduction for supported global `COUNT` and `countIf` shapes.
//!
//! This module owns checked row-count conversion, nullable `Int64` and `Bool`
//! chunk scans, operation-specific overflow reporting, ordered reduction, and
//! scheduler integration. The SQL engine retains query-shape eligibility and
//! physical column dispatch.

use super::aggregate_scheduler::{GlobalAggregateParallelism, run_grouped_aggregate};
use super::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operation {
    Count,
    CountIf,
}

impl Operation {
    const fn name(self) -> &'static str {
        match self {
            Self::Count => "COUNT",
            Self::CountIf => "countIf",
        }
    }

    const fn worker_name_prefix(self) -> &'static str {
        match self {
            Self::Count => "rusthouse-count-nullable-int64",
            Self::CountIf => "rusthouse-count-if",
        }
    }
}

pub(super) fn count_matched_rows(matched_rows: usize) -> Result<i64> {
    i64::try_from(matched_rows).map_err(|_| overflow(Operation::Count))
}

pub(super) fn count_present_values(present_count: u64) -> Result<i64> {
    i64::try_from(present_count).map_err(|_| overflow(Operation::Count))
}

pub(super) fn reduce_nullable_int64(
    values: &[Option<i64>],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
) -> Result<i64> {
    reduce_with_chunk(
        values,
        matching_rows,
        parallelism,
        Operation::Count,
        scan_nullable_int64_chunk,
    )
}

pub(super) fn reduce_count_if(
    values: &[bool],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
) -> Result<i64> {
    reduce_with_chunk(
        values,
        matching_rows,
        parallelism,
        Operation::CountIf,
        scan_count_if_chunk,
    )
}

fn reduce_with_chunk<T, C>(
    values: &[T],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
    operation: Operation,
    chunk: C,
) -> Result<i64>
where
    T: Sync,
    C: Fn(&[T], &[usize], Operation) -> Result<i64> + Sync,
{
    run_grouped_aggregate(
        matching_rows,
        parallelism,
        operation.worker_name_prefix(),
        |rows| chunk(values, rows, operation),
        |partials| reduce_ordered(partials, operation),
    )
}

fn scan_nullable_int64_chunk(
    values: &[Option<i64>],
    matching_rows: &[usize],
    operation: Operation,
) -> Result<i64> {
    matching_rows.iter().try_fold(0_i64, |count, row| {
        if values[*row].is_some() {
            checked_increment(count, operation)
        } else {
            Ok(count)
        }
    })
}

fn scan_count_if_chunk(
    values: &[bool],
    matching_rows: &[usize],
    operation: Operation,
) -> Result<i64> {
    matching_rows.iter().try_fold(0_i64, |count, row| {
        if values[*row] {
            checked_increment(count, operation)
        } else {
            Ok(count)
        }
    })
}

fn checked_increment(count: i64, operation: Operation) -> Result<i64> {
    count.checked_add(1).ok_or_else(|| overflow(operation))
}

fn reduce_ordered(partial_counts: Vec<i64>, operation: Operation) -> Result<i64> {
    partial_counts.into_iter().try_fold(0_i64, |total, count| {
        total.checked_add(count).ok_or_else(|| overflow(operation))
    })
}

fn overflow(operation: Operation) -> Error {
    Error::NumericOverflow(operation.name().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    use crate::batch::aggregate_scheduler::{
        GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD, TestGlobalAggregateWorkerBudget,
        parallel_aggregate_partition,
    };

    #[test]
    fn raw_slice_scans_preserve_selection_nulls_and_true_count() {
        let rows = [3, 1, 3, 0];
        assert_eq!(
            reduce_nullable_int64(
                &[Some(7), None, Some(99), Some(2)],
                &rows,
                GlobalAggregateParallelism::fixed(1),
            ),
            Ok(3)
        );
        assert_eq!(
            reduce_count_if(
                &[true, false, false, true],
                &rows,
                GlobalAggregateParallelism::fixed(1),
            ),
            Ok(3)
        );
    }

    #[test]
    fn checked_counts_preserve_operation_specific_overflow_contexts() {
        for (operation, expected) in [(Operation::Count, "COUNT"), (Operation::CountIf, "countIf")]
        {
            assert_eq!(
                checked_increment(i64::MAX, operation),
                Err(Error::NumericOverflow(expected.to_owned()))
            );
            assert_eq!(
                reduce_ordered(vec![i64::MAX, 1], operation),
                Err(Error::NumericOverflow(expected.to_owned()))
            );
        }

        #[cfg(target_pointer_width = "64")]
        assert_eq!(
            count_matched_rows(usize::try_from(i64::MAX).unwrap() + 1),
            Err(Error::NumericOverflow("COUNT".to_owned()))
        );
        assert_eq!(
            count_present_values(u64::try_from(i64::MAX).unwrap() + 1),
            Err(Error::NumericOverflow("COUNT".to_owned()))
        );
    }

    #[test]
    fn worker_failures_release_admission_and_repeat_complete_inputs() {
        static BUDGET: TestGlobalAggregateWorkerBudget =
            TestGlobalAggregateWorkerBudget::for_test(1);

        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut matching_rows = vec![0; row_count];
        let second_partition = parallel_aggregate_partition(&matching_rows, 2, 0).len();
        matching_rows[second_partition] = 1;
        matching_rows[row_count - 1] = 2;

        let nullable_worker_failed = AtomicBool::new(false);
        let nullable_count = reduce_with_chunk(
            &[Some(1), None, Some(17)],
            &matching_rows,
            GlobalAggregateParallelism::budgeted(
                GlobalAggregateParallelism::fixed(2).worker_cap(),
                &BUDGET,
            ),
            Operation::Count,
            |values, rows, operation| {
                if rows.len() == row_count {
                    assert_eq!(BUDGET.helpers_in_use(), 0);
                } else if std::thread::current().name() == Some("rusthouse-count-nullable-int64-1")
                {
                    nullable_worker_failed.store(true, Ordering::SeqCst);
                    panic!("injected nullable COUNT worker failure");
                }
                scan_nullable_int64_chunk(values, rows, operation)
            },
        )
        .expect("worker failure falls back to the complete nullable COUNT input");

        assert!(nullable_worker_failed.load(Ordering::SeqCst));
        assert_eq!(nullable_count, i64::try_from(row_count - 1).unwrap());
        assert_eq!(BUDGET.helpers_in_use(), 0);

        let count_if_worker_failed = AtomicBool::new(false);
        let count_if = reduce_with_chunk(
            &[true, false, true],
            &matching_rows,
            GlobalAggregateParallelism::budgeted(
                GlobalAggregateParallelism::fixed(2).worker_cap(),
                &BUDGET,
            ),
            Operation::CountIf,
            |values, rows, operation| {
                if rows.len() == row_count {
                    assert_eq!(BUDGET.helpers_in_use(), 0);
                } else if std::thread::current().name() == Some("rusthouse-count-if-1") {
                    count_if_worker_failed.store(true, Ordering::SeqCst);
                    panic!("injected countIf worker failure");
                }
                scan_count_if_chunk(values, rows, operation)
            },
        )
        .expect("worker failure falls back to the complete countIf input");

        assert!(count_if_worker_failed.load(Ordering::SeqCst));
        assert_eq!(count_if, i64::try_from(row_count - 1).unwrap());
        assert_eq!(BUDGET.helpers_in_use(), 0);
    }
}
