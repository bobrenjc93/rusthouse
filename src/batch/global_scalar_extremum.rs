//! Raw-slice reducers for supported global scalar extrema.
//!
//! This module owns deterministic chunking, scoped worker orchestration, and
//! ordered partial reduction for `Int64`, `Nullable(Int64)`, and `Float64`
//! slices. The SQL engine remains responsible for recognizing eligible query
//! shapes, resolving physical columns, and constructing typed result states.

use crate::batch::aggregate_scheduler::{GlobalAggregateParallelism, parallel_aggregate_partition};
use crate::batch::value::ValueRef;

pub(super) fn min_int64(
    values: &[i64],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
) -> Option<i64> {
    reduce_int64(values, matching_rows, parallelism, "min", i64::min)
}

pub(super) fn max_int64(
    values: &[i64],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
) -> Option<i64> {
    reduce_int64(values, matching_rows, parallelism, "max", i64::max)
}

pub(super) fn min_nullable_int64(
    values: &[Option<i64>],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
) -> Option<i64> {
    reduce_nullable_int64(values, matching_rows, parallelism, "min", i64::min)
}

pub(super) fn max_nullable_int64(
    values: &[Option<i64>],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
) -> Option<i64> {
    reduce_nullable_int64(values, matching_rows, parallelism, "max", i64::max)
}

pub(super) fn min_float64(
    values: &[f64],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
) -> Option<f64> {
    reduce_float64(
        values,
        matching_rows,
        parallelism,
        "min",
        first_float64_minimum,
    )
}

pub(super) fn max_float64(
    values: &[f64],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
) -> Option<f64> {
    reduce_float64(
        values,
        matching_rows,
        parallelism,
        "max",
        first_float64_maximum,
    )
}

fn reduce_int64<C>(
    values: &[i64],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
    worker_label: &'static str,
    compare: C,
) -> Option<i64>
where
    C: Fn(i64, i64) -> i64 + Sync,
{
    reduce_scalar(
        values,
        matching_rows,
        parallelism,
        worker_label,
        "int64",
        |value| Some(*value),
        compare,
    )
}

fn reduce_nullable_int64<C>(
    values: &[Option<i64>],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
    worker_label: &'static str,
    compare: C,
) -> Option<i64>
where
    C: Fn(i64, i64) -> i64 + Sync,
{
    reduce_scalar(
        values,
        matching_rows,
        parallelism,
        worker_label,
        "nullable-int64",
        |value| *value,
        compare,
    )
}

fn reduce_float64<C>(
    values: &[f64],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
    worker_label: &'static str,
    compare: C,
) -> Option<f64>
where
    C: Fn(f64, f64) -> f64 + Sync,
{
    reduce_scalar(
        values,
        matching_rows,
        parallelism,
        worker_label,
        "float64",
        |value| Some(*value),
        compare,
    )
}

fn reduce_scalar<T, E, M, C>(
    values: &[T],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
    worker_label: &'static str,
    worker_type_label: &'static str,
    map: M,
    compare: C,
) -> Option<E>
where
    T: Sync,
    E: Copy + Send,
    M: Fn(&T) -> Option<E> + Sync,
    C: Fn(E, E) -> E + Sync,
{
    let Some(admission) = parallelism.try_admit(matching_rows.len()) else {
        return scalar_chunk(values, matching_rows, &map, &compare);
    };

    // Each lane receives the same deterministic contiguous partition used by
    // the other global aggregates. Optional scalar partials are combined in
    // place, without allocating a partial-results collection. A failed spawn
    // or panic discards every partial and repeats the complete extremum on the
    // query thread after releasing process-wide admission.
    let helper_threads = admission.helper_threads();
    debug_assert!(helper_threads > 0);
    let worker_count = helper_threads.saturating_add(1);
    let map = &map;
    let compare = &compare;
    let parallel_result = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(helper_threads);
        let mut worker_failed = false;
        for chunk_index in 1..worker_count {
            let rows = parallel_aggregate_partition(matching_rows, worker_count, chunk_index);
            let spawn = std::thread::Builder::new()
                .name(format!(
                    "rusthouse-{worker_label}-{worker_type_label}-{chunk_index}"
                ))
                .spawn_scoped(scope, move || scalar_chunk(values, rows, map, compare));
            match spawn {
                Ok(handle) => handles.push(handle),
                Err(_) => {
                    worker_failed = true;
                    break;
                }
            }
        }

        let mut extremum = scalar_chunk(
            values,
            parallel_aggregate_partition(matching_rows, worker_count, 0),
            map,
            compare,
        );
        for handle in handles {
            match handle.join() {
                Ok(partial) => {
                    extremum = reduce_partials(extremum, partial, compare);
                }
                Err(_) => worker_failed = true,
            }
        }
        (!worker_failed).then_some(extremum)
    });
    drop(admission);
    parallel_result.unwrap_or_else(|| scalar_chunk(values, matching_rows, map, compare))
}

fn scalar_chunk<T, E, M, C>(
    values: &[T],
    matching_rows: &[usize],
    map: &M,
    compare: &C,
) -> Option<E>
where
    M: Fn(&T) -> Option<E>,
    C: Fn(E, E) -> E,
{
    matching_rows
        .iter()
        .filter_map(|row| map(&values[*row]))
        .reduce(compare)
}

fn reduce_partials<T, C>(left: Option<T>, right: Option<T>, compare: &C) -> Option<T>
where
    C: Fn(T, T) -> T,
{
    match (left, right) {
        (Some(left), Some(right)) => Some(compare(left, right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

pub(super) fn first_float64_minimum(left: f64, right: f64) -> f64 {
    if ValueRef::Float64(right) < ValueRef::Float64(left) {
        right
    } else {
        left
    }
}

fn first_float64_maximum(left: f64, right: f64) -> f64 {
    if ValueRef::Float64(right) > ValueRef::Float64(left) {
        right
    } else {
        left
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    use crate::batch::aggregate_scheduler::{
        GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD, TestGlobalAggregateWorkerBudget,
    };

    #[test]
    fn raw_slice_reducers_handle_selection_nulls_and_empty_inputs() {
        let parallelism = GlobalAggregateParallelism::fixed(1);
        let rows = [3, 1, 3, 0];

        assert_eq!(min_int64(&[7, -4, 99, 2], &rows, parallelism), Some(-4));
        assert_eq!(max_int64(&[7, -4, 99, 2], &rows, parallelism), Some(7));
        assert_eq!(min_int64(&[7], &[], parallelism), None);
        assert_eq!(max_int64(&[7], &[], parallelism), None);

        let nullable = [Some(7), None, Some(-4), None, Some(11)];
        assert_eq!(min_nullable_int64(&nullable, &[3, 1], parallelism), None);
        assert_eq!(max_nullable_int64(&nullable, &[3, 1], parallelism), None);
        assert_eq!(
            min_nullable_int64(&nullable, &[4, 1, 2], parallelism),
            Some(-4)
        );
        assert_eq!(
            max_nullable_int64(&nullable, &[4, 1, 2], parallelism),
            Some(11)
        );
    }

    #[test]
    fn float64_parallel_reduction_keeps_first_signed_zero() {
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut matching_rows = vec![0; row_count];
        let second_partition = parallel_aggregate_partition(&matching_rows, 2, 0).len();
        matching_rows[0] = 1;
        matching_rows[second_partition] = 2;

        let minimum = min_float64(
            &[1.0, -0.0, 0.0],
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
        )
        .expect("selected rows have a minimum");
        let maximum = max_float64(
            &[-1.0, 0.0, -0.0],
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
        )
        .expect("selected rows have a maximum");

        assert_eq!(minimum.to_bits(), (-0.0_f64).to_bits());
        assert_eq!(maximum.to_bits(), 0.0_f64.to_bits());
    }

    #[test]
    fn worker_panic_releases_admission_and_repeats_the_complete_reduction() {
        static BUDGET: TestGlobalAggregateWorkerBudget =
            TestGlobalAggregateWorkerBudget::for_test(1);

        let values = [9, i64::MIN];
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut matching_rows = vec![0; row_count];
        let second_partition = parallel_aggregate_partition(&matching_rows, 2, 0).len();
        matching_rows[second_partition] = 1;

        let minimum = reduce_int64(
            &values,
            &matching_rows,
            GlobalAggregateParallelism::budgeted(
                GlobalAggregateParallelism::fixed(2).worker_cap(),
                &BUDGET,
            ),
            "min",
            |left, right| {
                if std::thread::current().name() == Some("rusthouse-min-int64-1") {
                    panic!("injected MIN worker failure");
                }
                left.min(right)
            },
        );

        assert_eq!(minimum, Some(i64::MIN));
        assert_eq!(BUDGET.helpers_in_use(), 0);
    }

    #[test]
    fn nullable_and_float_worker_names_survive_the_module_boundary() {
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let matching_rows = vec![0; row_count];
        let saw_nullable_worker = AtomicBool::new(false);
        let saw_float_worker = AtomicBool::new(false);

        let nullable = reduce_nullable_int64(
            &[Some(3)],
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            "max",
            |left, right| {
                let current = std::thread::current();
                match current.name() {
                    Some(name) if name.starts_with("rusthouse-") => {
                        assert_eq!(name, "rusthouse-max-nullable-int64-1");
                        saw_nullable_worker.store(true, Ordering::Relaxed);
                    }
                    _ => {}
                }
                left.max(right)
            },
        );
        let float = reduce_float64(
            &[3.0],
            &matching_rows,
            GlobalAggregateParallelism::fixed(2),
            "min",
            |left, right| {
                let current = std::thread::current();
                match current.name() {
                    Some(name) if name.starts_with("rusthouse-") => {
                        assert_eq!(name, "rusthouse-min-float64-1");
                        saw_float_worker.store(true, Ordering::Relaxed);
                    }
                    _ => {}
                }
                first_float64_minimum(left, right)
            },
        );

        assert_eq!(nullable, Some(3));
        assert_eq!(float, Some(3.0));
        assert!(saw_nullable_worker.load(Ordering::Relaxed));
        assert!(saw_float_worker.load(Ordering::Relaxed));
    }

    #[test]
    fn optional_partials_reduce_in_order() {
        assert_eq!(reduce_partials(None, None, &i64::min), None);
        assert_eq!(reduce_partials(Some(4), None, &i64::min), Some(4));
        assert_eq!(reduce_partials(None, Some(-7), &i64::min), Some(-7));
        assert_eq!(
            reduce_partials(Some(-0.0_f64), Some(0.0), &first_float64_minimum).map(f64::to_bits),
            Some((-0.0_f64).to_bits())
        );
    }
}
