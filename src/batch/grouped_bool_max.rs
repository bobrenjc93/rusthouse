//! Private partial-state reduction for supported non-nullable `MAX` values grouped by `Bool`.

use super::aggregate_scheduler::{GlobalAggregateParallelism, run_grouped_aggregate};
use super::error::Result;
use super::global_scalar_extremum::first_float64_maximum;

const INT64_WORKER_NAME_PREFIX: &str = "rusthouse-group-bool-max-int64";
const FLOAT64_WORKER_NAME_PREFIX: &str = "rusthouse-group-bool-max-float64";

#[derive(Debug, PartialEq)]
pub(super) struct Partial<T> {
    false_max: Option<T>,
    true_max: Option<T>,
    first_seen: Option<bool>,
}

impl<T> Default for Partial<T> {
    fn default() -> Self {
        Self {
            false_max: None,
            true_max: None,
            first_seen: None,
        }
    }
}

impl<T: Copy> Partial<T> {
    fn slot(&self, group: bool) -> Option<T> {
        if group { self.true_max } else { self.false_max }
    }

    fn slot_mut(&mut self, group: bool) -> &mut Option<T> {
        if group {
            &mut self.true_max
        } else {
            &mut self.false_max
        }
    }

    pub(super) fn first_seen(&self) -> Option<bool> {
        self.first_seen
    }

    pub(super) fn present(&self, group: bool) -> bool {
        self.slot(group).is_some()
    }

    pub(super) fn maximum(&self, group: bool) -> T {
        self.slot(group)
            .expect("a present non-nullable group has a maximum")
    }

    fn observe<C>(&mut self, group: bool, value: T, first_maximum: &C)
    where
        C: Fn(T, T) -> T,
    {
        self.first_seen.get_or_insert(group);
        let maximum = self.slot_mut(group);
        *maximum = Some(maximum.map_or(value, |current| first_maximum(current, value)));
    }
}

pub(super) fn reduce_int64(
    group_values: &[bool],
    max_values: &[i64],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
) -> Result<Partial<i64>> {
    reduce_with_chunk(
        group_values,
        max_values,
        matching_rows,
        parallelism,
        INT64_WORKER_NAME_PREFIX,
        first_i64_maximum,
        scan_chunk,
    )
}

pub(super) fn reduce_float64(
    group_values: &[bool],
    max_values: &[f64],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
) -> Result<Partial<f64>> {
    reduce_with_chunk(
        group_values,
        max_values,
        matching_rows,
        parallelism,
        FLOAT64_WORKER_NAME_PREFIX,
        first_float64_maximum,
        scan_chunk,
    )
}

fn first_i64_maximum(left: i64, right: i64) -> i64 {
    if right > left { right } else { left }
}

fn reduce_with_chunk<T, M, C>(
    group_values: &[bool],
    max_values: &[T],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
    worker_name_prefix: &str,
    first_maximum: M,
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
        |rows| chunk(group_values, max_values, rows, &first_maximum),
        |partials| reduce_ordered(partials, &first_maximum),
    )
}

fn scan_chunk<T, C>(
    group_values: &[bool],
    max_values: &[T],
    matching_rows: &[usize],
    first_maximum: &C,
) -> Result<Partial<T>>
where
    T: Copy,
    C: Fn(T, T) -> T,
{
    let mut partial = Partial::default();
    for row in matching_rows {
        partial.observe(group_values[*row], max_values[*row], first_maximum);
    }
    Ok(partial)
}

fn reduce_ordered<T, C>(partials: Vec<Partial<T>>, first_maximum: &C) -> Result<Partial<T>>
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
            if let Some(maximum) = partial.false_max {
                total.observe(false, maximum, first_maximum);
            }
            if let Some(maximum) = partial.true_max {
                total.observe(true, maximum, first_maximum);
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
        let max_values = [i64::MIN, 7, i64::MAX];
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut matching_rows = vec![0; row_count];
        let second_partition = parallel_aggregate_partition(&matching_rows, 2, 0).len();
        matching_rows[second_partition] = 1;
        matching_rows[row_count - 1] = 2;

        let successful_parallel = reduce_int64(
            &group_values,
            &max_values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
        )
        .expect("deterministic parallel grouped MAX succeeds");
        let failed_parallel = reduce_with_chunk(
            &group_values,
            &max_values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            INT64_WORKER_NAME_PREFIX,
            first_i64_maximum,
            |group_values, max_values, rows, first_maximum| {
                if std::thread::current().name() == Some("rusthouse-group-bool-max-int64-1") {
                    panic!("injected grouped MAX worker failure");
                }
                scan_chunk(group_values, max_values, rows, first_maximum)
            },
        )
        .expect("worker failure falls back to the complete grouped MAX locally");

        let expected = Partial {
            false_max: Some(7),
            true_max: Some(i64::MAX),
            first_seen: Some(true),
        };
        assert_eq!(successful_parallel, expected);
        assert_eq!(failed_parallel, expected);
    }

    #[test]
    fn float64_worker_failure_repeats_complete_input_with_first_ties_and_group_order() {
        let group_values = [true, true, false];
        let max_values = [-0.0, 0.0, f64::MAX];
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut matching_rows = vec![0; row_count];
        let second_partition = parallel_aggregate_partition(&matching_rows, 2, 0).len();
        matching_rows[second_partition] = 1;
        matching_rows[row_count - 1] = 2;

        let successful_parallel = reduce_float64(
            &group_values,
            &max_values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
        )
        .expect("deterministic parallel grouped Float64 MAX succeeds");
        let failed_parallel = reduce_with_chunk(
            &group_values,
            &max_values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            FLOAT64_WORKER_NAME_PREFIX,
            first_float64_maximum,
            |group_values, max_values, rows, first_maximum| {
                if std::thread::current().name() == Some("rusthouse-group-bool-max-float64-1") {
                    panic!("injected grouped Float64 MAX worker failure");
                }
                scan_chunk(group_values, max_values, rows, first_maximum)
            },
        )
        .expect("worker failure falls back to the complete grouped Float64 MAX locally");

        for partial in [&successful_parallel, &failed_parallel] {
            assert_eq!(partial.first_seen(), Some(true));
            assert_eq!(partial.maximum(false), f64::MAX);
            assert_eq!(partial.maximum(true).to_bits(), (-0.0_f64).to_bits());
        }
    }
}
