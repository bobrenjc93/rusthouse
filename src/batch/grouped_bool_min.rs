//! Private partial-state reduction for supported non-nullable `MIN` values grouped by `Bool`.

use super::aggregate_scheduler::{GlobalAggregateParallelism, run_grouped_aggregate};
use super::error::Result;
use super::global_scalar_extremum::first_float64_minimum;

const INT64_WORKER_NAME_PREFIX: &str = "rusthouse-group-bool-min-int64";
const FLOAT64_WORKER_NAME_PREFIX: &str = "rusthouse-group-bool-min-float64";
const BOOL_WORKER_NAME_PREFIX: &str = "rusthouse-group-bool-min-bool";

#[derive(Debug, PartialEq)]
pub(super) struct Partial<T> {
    false_min: Option<T>,
    true_min: Option<T>,
    first_seen: Option<bool>,
}

impl<T> Default for Partial<T> {
    fn default() -> Self {
        Self {
            false_min: None,
            true_min: None,
            first_seen: None,
        }
    }
}

impl<T: Copy> Partial<T> {
    fn slot(&self, group: bool) -> Option<T> {
        if group { self.true_min } else { self.false_min }
    }

    fn slot_mut(&mut self, group: bool) -> &mut Option<T> {
        if group {
            &mut self.true_min
        } else {
            &mut self.false_min
        }
    }

    pub(super) fn first_seen(&self) -> Option<bool> {
        self.first_seen
    }

    pub(super) fn present(&self, group: bool) -> bool {
        self.slot(group).is_some()
    }

    pub(super) fn minimum(&self, group: bool) -> T {
        self.slot(group)
            .expect("a present non-nullable group has a minimum")
    }

    fn observe<C>(&mut self, group: bool, value: T, first_minimum: &C)
    where
        C: Fn(T, T) -> T,
    {
        self.first_seen.get_or_insert(group);
        let minimum = self.slot_mut(group);
        *minimum = Some(minimum.map_or(value, |current| first_minimum(current, value)));
    }
}

pub(super) fn reduce_int64(
    group_values: &[bool],
    min_values: &[i64],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
) -> Result<Partial<i64>> {
    reduce_with_chunk(
        group_values,
        min_values,
        matching_rows,
        parallelism,
        INT64_WORKER_NAME_PREFIX,
        first_i64_minimum,
        scan_chunk,
    )
}

pub(super) fn reduce_float64(
    group_values: &[bool],
    min_values: &[f64],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
) -> Result<Partial<f64>> {
    reduce_with_chunk(
        group_values,
        min_values,
        matching_rows,
        parallelism,
        FLOAT64_WORKER_NAME_PREFIX,
        first_float64_minimum,
        scan_chunk,
    )
}

pub(super) fn reduce_bool(
    group_values: &[bool],
    min_values: &[bool],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
) -> Result<Partial<bool>> {
    reduce_with_chunk(
        group_values,
        min_values,
        matching_rows,
        parallelism,
        BOOL_WORKER_NAME_PREFIX,
        first_bool_minimum,
        scan_chunk,
    )
}

fn first_i64_minimum(left: i64, right: i64) -> i64 {
    if right < left { right } else { left }
}

fn first_bool_minimum(left: bool, right: bool) -> bool {
    left && right
}

fn reduce_with_chunk<T, M, C>(
    group_values: &[bool],
    min_values: &[T],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
    worker_name_prefix: &str,
    first_minimum: M,
    chunk: C,
) -> Result<Partial<T>>
where
    T: Copy + Send + Sync,
    M: Fn(T, T) -> T + Sync,
    C: Fn(&[bool], &[T], &[usize], &M) -> Result<Partial<T>> + Sync,
{
    run_grouped_aggregate(
        matching_rows,
        parallelism,
        worker_name_prefix,
        |rows| chunk(group_values, min_values, rows, &first_minimum),
        |partials| reduce_ordered(partials, &first_minimum),
    )
}

fn scan_chunk<T, C>(
    group_values: &[bool],
    min_values: &[T],
    matching_rows: &[usize],
    first_minimum: &C,
) -> Result<Partial<T>>
where
    T: Copy,
    C: Fn(T, T) -> T,
{
    let mut partial = Partial::default();
    for row in matching_rows {
        partial.observe(group_values[*row], min_values[*row], first_minimum);
    }
    Ok(partial)
}

fn reduce_ordered<T, C>(partials: Vec<Partial<T>>, first_minimum: &C) -> Result<Partial<T>>
where
    T: Copy,
    C: Fn(T, T) -> T,
{
    Ok(partials
        .into_iter()
        .fold(Partial::default(), |mut total, partial| {
            if total.first_seen.is_none() {
                total.first_seen = partial.first_seen;
            }
            if let Some(minimum) = partial.false_min {
                total.observe(false, minimum, first_minimum);
            }
            if let Some(minimum) = partial.true_min {
                total.observe(true, minimum, first_minimum);
            }
            total
        }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::aggregate_scheduler::{
        GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD, parallel_aggregate_partition,
    };

    #[test]
    fn worker_failure_repeats_complete_input_and_preserves_extrema_and_first_group() {
        let group_values = [true, false, true];
        let min_values = [i64::MAX, 7, i64::MIN];
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut matching_rows = vec![0; row_count];
        let second_partition = parallel_aggregate_partition(&matching_rows, 2, 0).len();
        matching_rows[second_partition] = 1;
        matching_rows[row_count - 1] = 2;

        let successful_parallel = reduce_int64(
            &group_values,
            &min_values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
        )
        .expect("deterministic parallel grouped MIN succeeds");
        let failed_parallel = reduce_with_chunk(
            &group_values,
            &min_values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            INT64_WORKER_NAME_PREFIX,
            first_i64_minimum,
            |group_values, min_values, rows, first_minimum| {
                if std::thread::current().name() == Some("rusthouse-group-bool-min-int64-1") {
                    panic!("injected grouped MIN worker failure");
                }
                scan_chunk(group_values, min_values, rows, first_minimum)
            },
        )
        .expect("worker failure falls back to the complete grouped MIN locally");

        let expected = Partial {
            false_min: Some(7),
            true_min: Some(i64::MIN),
            first_seen: Some(true),
        };
        assert_eq!(successful_parallel, expected);
        assert_eq!(failed_parallel, expected);
    }

    #[test]
    fn float64_worker_failure_repeats_complete_input_with_first_ties_and_group_order() {
        let group_values = [true, true, false];
        let min_values = [-0.0, 0.0, f64::MIN];
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut matching_rows = vec![0; row_count];
        let second_partition = parallel_aggregate_partition(&matching_rows, 2, 0).len();
        matching_rows[second_partition] = 1;
        matching_rows[row_count - 1] = 2;

        let successful_parallel = reduce_float64(
            &group_values,
            &min_values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
        )
        .expect("deterministic parallel grouped Float64 MIN succeeds");
        let failed_parallel = reduce_with_chunk(
            &group_values,
            &min_values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            FLOAT64_WORKER_NAME_PREFIX,
            first_float64_minimum,
            |group_values, min_values, rows, first_minimum| {
                if std::thread::current().name() == Some("rusthouse-group-bool-min-float64-1") {
                    panic!("injected grouped Float64 MIN worker failure");
                }
                scan_chunk(group_values, min_values, rows, first_minimum)
            },
        )
        .expect("worker failure falls back to the complete grouped Float64 MIN locally");

        for partial in [&successful_parallel, &failed_parallel] {
            assert_eq!(partial.first_seen(), Some(true));
            assert_eq!(partial.minimum(false), f64::MIN);
            assert_eq!(partial.minimum(true).to_bits(), (-0.0_f64).to_bits());
        }
    }

    #[test]
    fn bool_worker_failure_repeats_complete_input_for_same_or_different_columns() {
        let group_values = [true, false, true];
        let min_values = [true, true, false];
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut matching_rows = vec![0; row_count];
        let second_partition = parallel_aggregate_partition(&matching_rows, 2, 0).len();
        matching_rows[second_partition] = 1;
        matching_rows[row_count - 1] = 2;

        let successful_parallel = reduce_bool(
            &group_values,
            &min_values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
        )
        .expect("deterministic parallel grouped Bool MIN succeeds");
        let failed_parallel = reduce_with_chunk(
            &group_values,
            &min_values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            BOOL_WORKER_NAME_PREFIX,
            first_bool_minimum,
            |group_values, min_values, rows, first_minimum| {
                if std::thread::current().name() == Some("rusthouse-group-bool-min-bool-1") {
                    panic!("injected grouped Bool MIN worker failure");
                }
                scan_chunk(group_values, min_values, rows, first_minimum)
            },
        )
        .expect("worker failure falls back to the complete grouped Bool MIN locally");

        let expected = Partial {
            false_min: Some(true),
            true_min: Some(false),
            first_seen: Some(true),
        };
        assert_eq!(successful_parallel, expected);
        assert_eq!(failed_parallel, expected);

        let same_column = reduce_bool(
            &group_values,
            &group_values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
        )
        .expect("same-column grouped Bool MIN succeeds");
        assert_eq!(
            same_column,
            Partial {
                false_min: Some(false),
                true_min: Some(true),
                first_seen: Some(true),
            }
        );
    }
}
