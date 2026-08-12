//! Raw-slice reduction for supported global scalar extrema.
//!
//! This module owns physical `Int64`, `Nullable(Int64)`, and `Float64` chunk
//! scans, deterministic partial reduction, and the complete admitted worker
//! lifecycle. The engine owns the other side of the boundary: it recognizes
//! eligible SQL shapes, selects a physical column, and constructs the final
//! aggregate state from the optional scalar returned here.

use crate::batch::aggregate_scheduler::{GlobalAggregateParallelism, parallel_aggregate_partition};
use crate::batch::value::ValueRef;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Extremum {
    Minimum,
    Maximum,
}

impl Extremum {
    const fn worker_label(self) -> &'static str {
        match self {
            Self::Minimum => "min",
            Self::Maximum => "max",
        }
    }
}

pub(super) fn reduce_int64(
    values: &[i64],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
    extremum: Extremum,
) -> Option<i64> {
    let compare: fn(i64, i64) -> i64 = match extremum {
        Extremum::Minimum => i64::min,
        Extremum::Maximum => i64::max,
    };
    reduce_int64_with(
        values,
        matching_rows,
        parallelism,
        extremum.worker_label(),
        compare,
    )
}

pub(super) fn reduce_nullable_int64(
    values: &[Option<i64>],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
    extremum: Extremum,
) -> Option<i64> {
    let compare: fn(i64, i64) -> i64 = match extremum {
        Extremum::Minimum => i64::min,
        Extremum::Maximum => i64::max,
    };
    reduce_nullable_int64_with(
        values,
        matching_rows,
        parallelism,
        extremum.worker_label(),
        compare,
    )
}

pub(super) fn reduce_float64(
    values: &[f64],
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
    extremum: Extremum,
) -> Option<f64> {
    let compare: fn(f64, f64) -> f64 = match extremum {
        Extremum::Minimum => first_float64_minimum,
        Extremum::Maximum => first_float64_maximum,
    };
    reduce_float64_with(
        values,
        matching_rows,
        parallelism,
        extremum.worker_label(),
        compare,
    )
}

pub(super) fn reduce_int64_with<C>(
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

pub(super) fn reduce_nullable_int64_with<C>(
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

pub(super) fn reduce_float64_with<C>(
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
        return reduce_chunk(values, matching_rows, &map, &compare);
    };

    // Lanes use the shared deterministic contiguous partition. Partials are
    // reduced in lane order, so a comparison that retains its left operand on
    // ties retains the first matching source value globally.
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
                .spawn_scoped(scope, move || reduce_chunk(values, rows, map, compare));
            match spawn {
                Ok(handle) => handles.push(handle),
                Err(_) => {
                    worker_failed = true;
                    break;
                }
            }
        }

        let mut extremum = reduce_chunk(
            values,
            parallel_aggregate_partition(matching_rows, worker_count, 0),
            map,
            compare,
        );
        for handle in handles {
            match handle.join() {
                Ok(partial) => extremum = reduce_partials(extremum, partial, compare),
                Err(_) => worker_failed = true,
            }
        }
        (!worker_failed).then_some(extremum)
    });

    // Admission must be released before a failed spawn or panicked helper
    // repeats the complete reduction on the query thread.
    drop(admission);
    parallel_result.unwrap_or_else(|| reduce_chunk(values, matching_rows, map, compare))
}

fn reduce_chunk<T, E, M, C>(
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

pub(super) fn reduce_partials<T, C>(left: Option<T>, right: Option<T>, compare: &C) -> Option<T>
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

pub(super) fn first_float64_maximum(left: f64, right: f64) -> f64 {
    if ValueRef::Float64(right) > ValueRef::Float64(left) {
        right
    } else {
        left
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::batch::aggregate_scheduler::{
        GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD, TestGlobalAggregateWorkerBudget,
    };

    #[test]
    fn raw_slice_policies_select_rows_and_ignore_nullable_absence() {
        let rows = [3, 0, 2];
        let sequential = GlobalAggregateParallelism::fixed(1);

        assert_eq!(
            reduce_int64(&[7, 99, -4, 12], &rows, sequential, Extremum::Minimum),
            Some(-4)
        );
        assert_eq!(
            reduce_int64(&[7, 99, -4, 12], &rows, sequential, Extremum::Maximum),
            Some(12)
        );
        assert_eq!(
            reduce_nullable_int64(
                &[None, Some(99), Some(-4), Some(12)],
                &rows,
                sequential,
                Extremum::Minimum,
            ),
            Some(-4)
        );
        assert_eq!(
            reduce_nullable_int64(
                &[None, Some(99), Some(-4), Some(12)],
                &[0],
                sequential,
                Extremum::Maximum,
            ),
            None
        );
        assert_eq!(
            reduce_float64(
                &[7.5, 99.0, -4.5, 12.25],
                &rows,
                sequential,
                Extremum::Maximum,
            ),
            Some(12.25)
        );
        assert_eq!(
            reduce_float64(&[], &[], sequential, Extremum::Minimum),
            None
        );
    }

    #[test]
    fn float64_parallel_ties_retain_the_first_signed_zero() {
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 2;
        let mut positive_first = vec![0; row_count];
        positive_first[row_count / 2] = 1;
        positive_first[row_count - 1] = 1;
        let parallel = GlobalAggregateParallelism::fixed(4);

        for extremum in [Extremum::Minimum, Extremum::Maximum] {
            let result = reduce_float64(&[0.0, -0.0], &positive_first, parallel, extremum)
                .expect("nonempty Float64 input has an extremum");
            assert_eq!(result.to_bits(), 0.0_f64.to_bits());

            let result = reduce_float64(&[-0.0, 0.0], &positive_first, parallel, extremum)
                .expect("nonempty Float64 input has an extremum");
            assert_eq!(result.to_bits(), (-0.0_f64).to_bits());
        }
    }

    #[test]
    fn optional_partials_reduce_without_losing_empty_lanes() {
        assert_eq!(reduce_partials(None, None, &i64::min), None);
        assert_eq!(reduce_partials(Some(4), None, &i64::min), Some(4));
        assert_eq!(reduce_partials(None, Some(-7), &i64::min), Some(-7));
        assert_eq!(reduce_partials(Some(4), Some(-7), &i64::min), Some(-7));
        assert_eq!(reduce_partials(Some(4), Some(-7), &i64::max), Some(4));
    }

    #[test]
    fn worker_panic_uses_the_stable_name_and_releases_admission_before_fallback() {
        static BUDGET: TestGlobalAggregateWorkerBudget =
            TestGlobalAggregateWorkerBudget::for_test(1);

        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let mut matching_rows = vec![0; row_count];
        matching_rows[row_count - 1] = 1;
        let fallback_observed = AtomicBool::new(false);
        let parallelism = GlobalAggregateParallelism::budgeted(
            NonZeroUsize::new(2).expect("test cap is nonzero"),
            &BUDGET,
        );

        let result = reduce_int64_with(
            &[9, i64::MIN],
            &matching_rows,
            parallelism,
            "min",
            |left, right| {
                if std::thread::current().name() == Some("rusthouse-min-int64-1") {
                    panic!("injected extremum worker failure");
                }
                if BUDGET.helpers_in_use() == 0 {
                    fallback_observed.store(true, Ordering::Release);
                }
                left.min(right)
            },
        );

        assert_eq!(result, Some(i64::MIN));
        assert!(fallback_observed.load(Ordering::Acquire));
        assert_eq!(BUDGET.helpers_in_use(), 0);
    }
}
