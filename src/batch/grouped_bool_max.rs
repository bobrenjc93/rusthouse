//! Private partial-state reduction for non-nullable numeric `MAX` grouped by `Bool`.

use super::aggregate_scheduler::{GlobalAggregateParallelism, run_grouped_aggregate};
use super::error::Result;
use super::value::ValueRef;

const INT64_WORKER_NAME_PREFIX: &str = "rusthouse-group-bool-max-int64";
const FLOAT64_WORKER_NAME_PREFIX: &str = "rusthouse-group-bool-max-float64";

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Partial {
    false_max: Option<i64>,
    true_max: Option<i64>,
    first_seen: Option<bool>,
}

impl Partial {
    fn slot(&self, group: bool) -> Option<i64> {
        if group { self.true_max } else { self.false_max }
    }

    fn slot_mut(&mut self, group: bool) -> &mut Option<i64> {
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

    pub(super) fn maximum(&self, group: bool) -> i64 {
        self.slot(group)
            .expect("a present non-nullable Int64 group has a maximum")
    }

    fn observe(&mut self, group: bool, value: i64) {
        self.first_seen.get_or_insert(group);
        let maximum = self.slot_mut(group);
        *maximum = Some(maximum.map_or(value, |current| current.max(value)));
    }
}

pub(super) fn reduce(
    group_values: &[bool],
    max_values: &[i64],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
) -> Result<Partial> {
    reduce_with_chunk(
        group_values,
        max_values,
        matching_rows,
        parallelism,
        scan_chunk,
    )
}

fn reduce_with_chunk<C>(
    group_values: &[bool],
    max_values: &[i64],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
    chunk: C,
) -> Result<Partial>
where
    C: Fn(&[bool], &[i64], &[usize]) -> Result<Partial> + Sync,
{
    run_grouped_aggregate(
        matching_rows,
        parallelism,
        INT64_WORKER_NAME_PREFIX,
        |rows| chunk(group_values, max_values, rows),
        reduce_ordered,
    )
}

fn scan_chunk(
    group_values: &[bool],
    max_values: &[i64],
    matching_rows: &[usize],
) -> Result<Partial> {
    let mut partial = Partial::default();
    for row in matching_rows {
        partial.observe(group_values[*row], max_values[*row]);
    }
    Ok(partial)
}

fn reduce_ordered(partials: Vec<Partial>) -> Result<Partial> {
    Ok(partials
        .into_iter()
        .fold(Partial::default(), |mut total, partial| {
            if total.first_seen.is_none() {
                total.first_seen = partial.first_seen;
            }
            if let Some(maximum) = partial.false_max {
                total.observe(false, maximum);
            }
            if let Some(maximum) = partial.true_max {
                total.observe(true, maximum);
            }
            total
        }))
}

#[derive(Debug, Default, PartialEq)]
pub(super) struct Float64Partial {
    false_max: Option<f64>,
    true_max: Option<f64>,
    first_seen: Option<bool>,
}

impl Float64Partial {
    fn slot(&self, group: bool) -> Option<f64> {
        if group { self.true_max } else { self.false_max }
    }

    fn slot_mut(&mut self, group: bool) -> &mut Option<f64> {
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

    pub(super) fn maximum(&self, group: bool) -> f64 {
        self.slot(group)
            .expect("a present non-nullable Float64 group has a maximum")
    }

    fn observe(&mut self, group: bool, value: f64) {
        self.first_seen.get_or_insert(group);
        let maximum = self.slot_mut(group);
        if maximum.is_none_or(|current| ValueRef::Float64(value) > ValueRef::Float64(current)) {
            *maximum = Some(value);
        }
    }
}

pub(super) fn reduce_float64(
    group_values: &[bool],
    max_values: &[f64],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
) -> Result<Float64Partial> {
    reduce_float64_with_chunk(
        group_values,
        max_values,
        matching_rows,
        parallelism,
        scan_float64_chunk,
    )
}

fn reduce_float64_with_chunk<C>(
    group_values: &[bool],
    max_values: &[f64],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
    chunk: C,
) -> Result<Float64Partial>
where
    C: Fn(&[bool], &[f64], &[usize]) -> Result<Float64Partial> + Sync,
{
    run_grouped_aggregate(
        matching_rows,
        parallelism,
        FLOAT64_WORKER_NAME_PREFIX,
        |rows| chunk(group_values, max_values, rows),
        reduce_float64_ordered,
    )
}

fn scan_float64_chunk(
    group_values: &[bool],
    max_values: &[f64],
    matching_rows: &[usize],
) -> Result<Float64Partial> {
    let mut partial = Float64Partial::default();
    for row in matching_rows {
        partial.observe(group_values[*row], max_values[*row]);
    }
    Ok(partial)
}

fn reduce_float64_ordered(partials: Vec<Float64Partial>) -> Result<Float64Partial> {
    Ok(partials
        .into_iter()
        .fold(Float64Partial::default(), |mut total, partial| {
            if total.first_seen.is_none() {
                total.first_seen = partial.first_seen;
            }
            if let Some(maximum) = partial.false_max {
                total.observe(false, maximum);
            }
            if let Some(maximum) = partial.true_max {
                total.observe(true, maximum);
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

        let successful_parallel = reduce(
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
            |group_values, max_values, rows| {
                if std::thread::current().name() == Some("rusthouse-group-bool-max-int64-1") {
                    panic!("injected grouped MAX worker failure");
                }
                scan_chunk(group_values, max_values, rows)
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
    fn float64_parallel_reduction_preserves_first_signed_zero_for_each_group() {
        let group_values = [true, true, false, false];
        let max_values = [-0.0, 0.0, 0.0, -0.0];
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut matching_rows = vec![0; row_count];
        let second_partition = parallel_aggregate_partition(&matching_rows, 2, 0).len();
        matching_rows[second_partition] = 1;
        matching_rows[second_partition + 1] = 2;
        matching_rows[row_count - 1] = 3;

        let partial = reduce_float64(
            &group_values,
            &max_values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
        )
        .expect("deterministic parallel grouped Float64 MAX succeeds");

        assert_eq!(partial.first_seen(), Some(true));
        assert_eq!(partial.maximum(true).to_bits(), (-0.0_f64).to_bits());
        assert_eq!(partial.maximum(false).to_bits(), 0.0_f64.to_bits());
    }

    #[test]
    fn float64_worker_failure_repeats_complete_input_and_preserves_extrema() {
        let group_values = [true, false, true];
        let max_values = [f64::MIN, -0.0, f64::MAX];
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
        let failed_parallel = reduce_float64_with_chunk(
            &group_values,
            &max_values,
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            |group_values, max_values, rows| {
                if std::thread::current().name() == Some("rusthouse-group-bool-max-float64-1") {
                    panic!("injected grouped Float64 MAX worker failure");
                }
                scan_float64_chunk(group_values, max_values, rows)
            },
        )
        .expect("worker failure falls back to the complete grouped Float64 MAX locally");

        assert_eq!(failed_parallel, successful_parallel);
        assert_eq!(failed_parallel.first_seen(), Some(true));
        assert_eq!(
            failed_parallel.maximum(false).to_bits(),
            (-0.0_f64).to_bits()
        );
        assert_eq!(failed_parallel.maximum(true), f64::MAX);
    }
}
