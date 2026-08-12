//! Private partial-state reduction for `COUNT` and `countIf` grouped by `Bool`.

use super::aggregate_scheduler::{GlobalAggregateParallelism, run_grouped_aggregate};
use super::error::{Error, Result};

const WORKER_NAME_PREFIX: &str = "rusthouse-group-bool-count";

#[derive(Debug, Clone, Copy)]
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
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Partial {
    false_rows: i64,
    true_rows: i64,
    false_count: i64,
    true_count: i64,
    first_seen: Option<bool>,
}

impl Partial {
    pub(super) fn row_count(&self, group: bool) -> i64 {
        if group {
            self.true_rows
        } else {
            self.false_rows
        }
    }

    pub(super) fn count(&self, group: bool) -> i64 {
        if group {
            self.true_count
        } else {
            self.false_count
        }
    }

    pub(super) fn first_seen(&self) -> Option<bool> {
        self.first_seen
    }

    fn observe(&mut self, group: bool, counted: bool, operation: Operation) -> Result<()> {
        self.first_seen.get_or_insert(group);
        let rows = if group {
            &mut self.true_rows
        } else {
            &mut self.false_rows
        };
        *rows = rows
            .checked_add(1)
            .ok_or_else(|| Error::NumericOverflow(operation.name().to_owned()))?;
        if counted {
            let count = if group {
                &mut self.true_count
            } else {
                &mut self.false_count
            };
            *count = count
                .checked_add(1)
                .ok_or_else(|| Error::NumericOverflow(operation.name().to_owned()))?;
        }
        Ok(())
    }
}

/// Reduces `COUNT()`/`COUNT(*)` rows, also used for physical non-nullable
/// `COUNT(column)` because every selected row contributes.
pub(super) fn reduce_rows(
    group_values: &[bool],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
) -> Result<Partial> {
    reduce_with_chunk(
        group_values,
        matching_rows,
        parallelism,
        Operation::Count,
        scan_rows,
    )
}

pub(super) fn reduce_nullable_int64(
    group_values: &[bool],
    count_values: &[Option<i64>],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
) -> Result<Partial> {
    reduce_with_chunk(
        group_values,
        matching_rows,
        parallelism,
        Operation::Count,
        |group_values, rows, operation| {
            scan_nullable_int64(group_values, count_values, rows, operation)
        },
    )
}

pub(super) fn reduce_count_if(
    group_values: &[bool],
    count_values: &[bool],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
) -> Result<Partial> {
    reduce_with_chunk(
        group_values,
        matching_rows,
        parallelism,
        Operation::CountIf,
        |group_values, rows, operation| scan_count_if(group_values, count_values, rows, operation),
    )
}

fn reduce_with_chunk<C>(
    group_values: &[bool],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
    operation: Operation,
    chunk: C,
) -> Result<Partial>
where
    C: Fn(&[bool], &[usize], Operation) -> Result<Partial> + Sync,
{
    run_grouped_aggregate(
        matching_rows,
        parallelism,
        WORKER_NAME_PREFIX,
        |rows| chunk(group_values, rows, operation),
        |partials| reduce_ordered(partials, operation),
    )
}

fn scan_rows(
    group_values: &[bool],
    matching_rows: &[usize],
    operation: Operation,
) -> Result<Partial> {
    let mut partial = Partial::default();
    for row in matching_rows {
        partial.observe(group_values[*row], true, operation)?;
    }
    Ok(partial)
}

fn scan_nullable_int64(
    group_values: &[bool],
    count_values: &[Option<i64>],
    matching_rows: &[usize],
    operation: Operation,
) -> Result<Partial> {
    let mut partial = Partial::default();
    for row in matching_rows {
        partial.observe(group_values[*row], count_values[*row].is_some(), operation)?;
    }
    Ok(partial)
}

fn scan_count_if(
    group_values: &[bool],
    count_values: &[bool],
    matching_rows: &[usize],
    operation: Operation,
) -> Result<Partial> {
    let mut partial = Partial::default();
    for row in matching_rows {
        partial.observe(group_values[*row], count_values[*row], operation)?;
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
            total.false_rows = total
                .false_rows
                .checked_add(partial.false_rows)
                .ok_or_else(|| Error::NumericOverflow(operation.name().to_owned()))?;
            total.true_rows = total
                .true_rows
                .checked_add(partial.true_rows)
                .ok_or_else(|| Error::NumericOverflow(operation.name().to_owned()))?;
            total.false_count = total
                .false_count
                .checked_add(partial.false_count)
                .ok_or_else(|| Error::NumericOverflow(operation.name().to_owned()))?;
            total.true_count = total
                .true_count
                .checked_add(partial.true_count)
                .ok_or_else(|| Error::NumericOverflow(operation.name().to_owned()))?;
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
    fn row_count_worker_failure_repeats_complete_grouping_locally() {
        let values = [true, false];
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut matching_rows = vec![1; row_count];
        let second_partition = parallel_aggregate_partition(&matching_rows, 2, 0).len();
        matching_rows[second_partition] = 0;

        let successful_parallel = reduce_rows(
            &values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
        )
        .expect("deterministic parallel grouping succeeds");
        let failed_parallel = reduce_with_chunk(
            &values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            Operation::Count,
            |values, rows, operation| {
                if std::thread::current().name() == Some("rusthouse-group-bool-count-1") {
                    panic!("injected grouped Bool COUNT worker failure");
                }
                scan_rows(values, rows, operation)
            },
        )
        .expect("worker failure falls back to complete local grouping");

        let expected = Partial {
            false_rows: i64::try_from(row_count - 1).unwrap(),
            true_rows: 1,
            false_count: i64::try_from(row_count - 1).unwrap(),
            true_count: 1,
            first_seen: Some(false),
        };
        assert_eq!(successful_parallel, expected);
        assert_eq!(failed_parallel, expected);
    }

    #[test]
    fn nullable_count_worker_failure_repeats_complete_input_locally() {
        let group_values = [true, false, false];
        let count_values = [None, Some(17), None];
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut matching_rows = vec![0; row_count];
        let second_partition = parallel_aggregate_partition(&matching_rows, 2, 0).len();
        matching_rows[second_partition] = 1;
        matching_rows[row_count - 1] = 2;

        let successful_parallel = reduce_nullable_int64(
            &group_values,
            &count_values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
        )
        .expect("deterministic parallel grouped nullable COUNT succeeds");
        let failed_parallel = reduce_with_chunk(
            &group_values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            Operation::Count,
            |group_values, rows, operation| {
                if std::thread::current().name() == Some("rusthouse-group-bool-count-1") {
                    panic!("injected grouped nullable COUNT worker failure");
                }
                scan_nullable_int64(group_values, &count_values, rows, operation)
            },
        )
        .expect("worker failure falls back to a complete local grouped nullable COUNT");

        let expected = Partial {
            false_rows: 2,
            true_rows: i64::try_from(row_count - 2).unwrap(),
            false_count: 1,
            true_count: 0,
            first_seen: Some(true),
        };
        assert_eq!(successful_parallel, expected);
        assert_eq!(failed_parallel, expected);
    }

    #[test]
    fn count_if_worker_failure_repeats_complete_input_locally() {
        let group_values = [true, false, false];
        let count_values = [false, true, false];
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut matching_rows = vec![0; row_count];
        let second_partition = parallel_aggregate_partition(&matching_rows, 2, 0).len();
        matching_rows[second_partition] = 1;
        matching_rows[row_count - 1] = 2;

        let successful_parallel = reduce_count_if(
            &group_values,
            &count_values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
        )
        .expect("deterministic parallel grouped countIf succeeds");
        let failed_parallel = reduce_with_chunk(
            &group_values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            Operation::CountIf,
            |group_values, rows, operation| {
                if std::thread::current().name() == Some("rusthouse-group-bool-count-1") {
                    panic!("injected grouped countIf worker failure");
                }
                scan_count_if(group_values, &count_values, rows, operation)
            },
        )
        .expect("worker failure falls back to a complete local grouped countIf");

        let expected = Partial {
            false_rows: 2,
            true_rows: i64::try_from(row_count - 2).unwrap(),
            false_count: 1,
            true_count: 0,
            first_seen: Some(true),
        };
        assert_eq!(successful_parallel, expected);
        assert_eq!(failed_parallel, expected);
    }

    #[test]
    fn count_if_chunk_overflow_reports_count_if() {
        let error = reduce_with_chunk(
            &[true],
            &[0],
            GlobalAggregateParallelism::fixed(1),
            Operation::CountIf,
            |_, _, operation| {
                let mut partial = Partial {
                    true_count: i64::MAX,
                    ..Partial::default()
                };
                partial.observe(true, true, operation)?;
                Ok(partial)
            },
        );

        assert_eq!(
            error,
            Err(Error::NumericOverflow("countIf".to_owned())),
            "a synthetic sequential chunk needs no enormous input allocation"
        );
    }

    #[test]
    fn count_if_cross_partial_overflows_report_count_if() {
        for partials in [
            vec![
                Partial {
                    false_rows: i64::MAX,
                    first_seen: Some(false),
                    ..Partial::default()
                },
                Partial {
                    false_rows: 1,
                    first_seen: Some(false),
                    ..Partial::default()
                },
            ],
            vec![
                Partial {
                    true_count: i64::MAX,
                    first_seen: Some(true),
                    ..Partial::default()
                },
                Partial {
                    true_count: 1,
                    first_seen: Some(true),
                    ..Partial::default()
                },
            ],
        ] {
            assert_eq!(
                reduce_ordered(partials, Operation::CountIf),
                Err(Error::NumericOverflow("countIf".to_owned()))
            );
        }
    }

    #[test]
    fn count_overflows_still_report_count() {
        let mut partial = Partial {
            true_count: i64::MAX,
            ..Partial::default()
        };
        assert_eq!(
            partial.observe(true, true, Operation::Count),
            Err(Error::NumericOverflow("COUNT".to_owned()))
        );

        assert_eq!(
            reduce_ordered(
                vec![
                    Partial {
                        false_count: i64::MAX,
                        ..Partial::default()
                    },
                    Partial {
                        false_count: 1,
                        ..Partial::default()
                    },
                ],
                Operation::Count,
            ),
            Err(Error::NumericOverflow("COUNT".to_owned()))
        );
    }
}
