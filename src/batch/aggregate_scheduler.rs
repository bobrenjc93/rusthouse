//! Private scheduling and admission for parallel aggregate reducers.

use std::num::NonZeroUsize;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Minimum matched rows that a supported aggregate evaluates sequentially.
///
/// Parallel evaluation is considered only when the matched row count is
/// strictly greater than this threshold.
pub const GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD: usize = 256 * 1024;
/// Target matched rows per supported aggregate computation lane.
pub const GLOBAL_AGGREGATE_PARALLEL_ROWS_PER_WORKER: usize = 128 * 1024;
/// Maximum computation lanes used by one supported aggregate.
///
/// The executor also caps this value by [`std::thread::available_parallelism`]
/// and admits helper threads through one process-wide budget.
pub const MAX_GLOBAL_AGGREGATE_PARALLEL_WORKERS: usize = 16;
/// Default per-database computation-lane cap for supported aggregates.
///
/// The process-wide admission budget and available hardware may lower the
/// effective lane count further.
pub const DEFAULT_GLOBAL_AGGREGATE_WORKER_CAP: usize = MAX_GLOBAL_AGGREGATE_PARALLEL_WORKERS;

/// Backwards-compatible name for [`GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD`].
pub const COUNT_IF_PARALLEL_ROW_THRESHOLD: usize = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD;
/// Backwards-compatible name for [`GLOBAL_AGGREGATE_PARALLEL_ROWS_PER_WORKER`].
pub const COUNT_IF_PARALLEL_ROWS_PER_WORKER: usize = GLOBAL_AGGREGATE_PARALLEL_ROWS_PER_WORKER;
/// Backwards-compatible name for [`MAX_GLOBAL_AGGREGATE_PARALLEL_WORKERS`].
pub const MAX_COUNT_IF_PARALLEL_WORKERS: usize = MAX_GLOBAL_AGGREGATE_PARALLEL_WORKERS;

#[derive(Debug, Clone, Copy)]
pub(super) struct GlobalAggregateParallelism {
    worker_cap: NonZeroUsize,
    source: GlobalAggregateParallelismSource,
}

#[derive(Debug, Clone, Copy)]
enum GlobalAggregateParallelismSource {
    System,
    #[cfg(test)]
    Fixed(usize),
    #[cfg(test)]
    Budgeted(&'static GlobalAggregateWorkerBudget),
}

impl GlobalAggregateParallelism {
    pub(super) const fn system(worker_cap: NonZeroUsize) -> Self {
        Self {
            worker_cap,
            source: GlobalAggregateParallelismSource::System,
        }
    }

    pub(super) const fn worker_cap(self) -> NonZeroUsize {
        self.worker_cap
    }

    pub(super) fn set_worker_cap(&mut self, worker_cap: NonZeroUsize) -> NonZeroUsize {
        std::mem::replace(&mut self.worker_cap, worker_cap)
    }

    #[cfg(test)]
    pub(super) fn fixed(workers: usize) -> Self {
        Self {
            worker_cap: NonZeroUsize::new(workers).expect("fixed workers must be nonzero"),
            source: GlobalAggregateParallelismSource::Fixed(workers),
        }
    }

    #[cfg(test)]
    pub(super) const fn budgeted(
        worker_cap: NonZeroUsize,
        budget: &'static TestGlobalAggregateWorkerBudget,
    ) -> Self {
        Self {
            worker_cap,
            source: GlobalAggregateParallelismSource::Budgeted(&budget.budget),
        }
    }

    fn worker_count(self, matched_rows: usize) -> usize {
        if matched_rows <= GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD {
            return 1;
        }

        let worker_limit = match self.source {
            GlobalAggregateParallelismSource::System => {
                global_aggregate_worker_budget().worker_limit()
            }
            #[cfg(test)]
            GlobalAggregateParallelismSource::Fixed(workers) => workers,
            #[cfg(test)]
            GlobalAggregateParallelismSource::Budgeted(budget) => budget.worker_limit(),
        };
        let useful_workers = matched_rows.div_ceil(GLOBAL_AGGREGATE_PARALLEL_ROWS_PER_WORKER);
        worker_limit
            .min(self.worker_cap.get())
            .clamp(1, MAX_GLOBAL_AGGREGATE_PARALLEL_WORKERS)
            .min(useful_workers)
            .min(matched_rows)
    }

    pub(super) fn with_request_worker_cap(self, max_threads: usize) -> Self {
        if max_threads == 0 {
            return self;
        }

        Self {
            worker_cap: NonZeroUsize::new(self.worker_cap.get().min(max_threads))
                .expect("a nonzero request cap and database cap have a nonzero minimum"),
            ..self
        }
    }

    pub(super) fn try_admit(self, matched_rows: usize) -> Option<GlobalAggregateWorkerAdmission> {
        let helper_threads = self.worker_count(matched_rows).saturating_sub(1);
        if helper_threads == 0 {
            return None;
        }

        match self.source {
            GlobalAggregateParallelismSource::System => global_aggregate_worker_budget()
                .try_acquire(helper_threads)
                .map(GlobalAggregateWorkerAdmission::budgeted),
            #[cfg(test)]
            GlobalAggregateParallelismSource::Fixed(_) => {
                Some(GlobalAggregateWorkerAdmission::fixed(helper_threads))
            }
            #[cfg(test)]
            GlobalAggregateParallelismSource::Budgeted(budget) => budget
                .try_acquire(helper_threads)
                .map(GlobalAggregateWorkerAdmission::budgeted),
        }
    }
}

#[derive(Debug)]
struct GlobalAggregateWorkerBudget {
    helper_limit: usize,
    helpers_in_use: AtomicUsize,
    #[cfg(test)]
    peak_helpers_in_use: AtomicUsize,
}

impl GlobalAggregateWorkerBudget {
    const fn new(helper_limit: usize) -> Self {
        Self {
            helper_limit,
            helpers_in_use: AtomicUsize::new(0),
            #[cfg(test)]
            peak_helpers_in_use: AtomicUsize::new(0),
        }
    }

    fn worker_limit(&self) -> usize {
        self.helper_limit.saturating_add(1)
    }

    fn try_acquire(&'static self, requested: usize) -> Option<GlobalAggregateWorkerPermit> {
        let mut in_use = self.helpers_in_use.load(Ordering::Acquire);
        loop {
            let acquired = requested.min(self.helper_limit.saturating_sub(in_use));
            if acquired == 0 {
                return None;
            }
            let next = in_use.saturating_add(acquired);
            match self.helpers_in_use.compare_exchange_weak(
                in_use,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    #[cfg(test)]
                    self.peak_helpers_in_use.fetch_max(next, Ordering::Relaxed);
                    return Some(GlobalAggregateWorkerPermit {
                        budget: self,
                        helper_threads: acquired,
                    });
                }
                Err(actual) => in_use = actual,
            }
        }
    }

    #[cfg(test)]
    fn helpers_in_use(&self) -> usize {
        self.helpers_in_use.load(Ordering::Acquire)
    }

    #[cfg(test)]
    fn peak_helpers_in_use(&self) -> usize {
        self.peak_helpers_in_use.load(Ordering::Acquire)
    }

    #[cfg(test)]
    fn reset_peak(&self) {
        assert_eq!(self.helpers_in_use(), 0);
        self.peak_helpers_in_use.store(0, Ordering::Release);
    }
}

#[derive(Debug)]
struct GlobalAggregateWorkerPermit {
    budget: &'static GlobalAggregateWorkerBudget,
    helper_threads: usize,
}

impl Drop for GlobalAggregateWorkerPermit {
    fn drop(&mut self) {
        let previous = self
            .budget
            .helpers_in_use
            .fetch_sub(self.helper_threads, Ordering::AcqRel);
        debug_assert!(previous >= self.helper_threads);
    }
}

#[derive(Debug)]
enum GlobalAggregateWorkerAdmissionKind {
    Budgeted(GlobalAggregateWorkerPermit),
    #[cfg(test)]
    Fixed(usize),
}

#[derive(Debug)]
pub(super) struct GlobalAggregateWorkerAdmission {
    kind: GlobalAggregateWorkerAdmissionKind,
}

impl GlobalAggregateWorkerAdmission {
    fn budgeted(permit: GlobalAggregateWorkerPermit) -> Self {
        Self {
            kind: GlobalAggregateWorkerAdmissionKind::Budgeted(permit),
        }
    }

    #[cfg(test)]
    fn fixed(helper_threads: usize) -> Self {
        Self {
            kind: GlobalAggregateWorkerAdmissionKind::Fixed(helper_threads),
        }
    }

    pub(super) fn helper_threads(&self) -> usize {
        match &self.kind {
            GlobalAggregateWorkerAdmissionKind::Budgeted(permit) => permit.helper_threads,
            #[cfg(test)]
            GlobalAggregateWorkerAdmissionKind::Fixed(helper_threads) => *helper_threads,
        }
    }
}

#[cfg(test)]
#[derive(Debug)]
pub(super) struct TestGlobalAggregateWorkerBudget {
    budget: GlobalAggregateWorkerBudget,
}

#[cfg(test)]
impl TestGlobalAggregateWorkerBudget {
    pub(super) const fn for_test(helper_limit: usize) -> Self {
        Self {
            budget: GlobalAggregateWorkerBudget::new(helper_limit),
        }
    }

    pub(super) fn acquire_for_test(
        &'static self,
        requested: usize,
    ) -> Option<TestGlobalAggregateWorkerPermit> {
        self.budget
            .try_acquire(requested)
            .map(|permit| TestGlobalAggregateWorkerPermit { _permit: permit })
    }

    pub(super) fn helper_limit(&self) -> usize {
        self.budget.helper_limit
    }

    pub(super) fn helpers_in_use(&self) -> usize {
        self.budget.helpers_in_use()
    }

    pub(super) fn peak_helpers_in_use(&self) -> usize {
        self.budget.peak_helpers_in_use()
    }

    pub(super) fn reset_peak(&self) {
        self.budget.reset_peak();
    }
}

#[cfg(test)]
#[derive(Debug)]
pub(super) struct TestGlobalAggregateWorkerPermit {
    _permit: GlobalAggregateWorkerPermit,
}

fn global_aggregate_worker_budget() -> &'static GlobalAggregateWorkerBudget {
    static BUDGET: OnceLock<GlobalAggregateWorkerBudget> = OnceLock::new();
    BUDGET.get_or_init(|| {
        let worker_limit = std::thread::available_parallelism()
            .map_or(1, |value| value.get())
            .min(MAX_GLOBAL_AGGREGATE_PARALLEL_WORKERS);
        GlobalAggregateWorkerBudget::new(worker_limit.saturating_sub(1))
    })
}

pub(super) fn parallel_aggregate_partition(
    matching_rows: &[usize],
    worker_count: usize,
    worker_index: usize,
) -> &[usize] {
    debug_assert!(worker_count > 0);
    debug_assert!(worker_index < worker_count);
    let rows_per_worker = matching_rows.len() / worker_count;
    let workers_with_extra_row = matching_rows.len() % worker_count;
    let start = worker_index * rows_per_worker + worker_index.min(workers_with_extra_row);
    let partition_len = rows_per_worker + usize::from(worker_index < workers_with_extra_row);
    &matching_rows[start..start + partition_len]
}

/// Runs one grouped aggregate over admitted, deterministically ordered lanes.
///
/// SQL shape recognition, partial construction, and partial reduction remain
/// caller policy. This driver owns only worker admission and lifecycle. If a
/// helper cannot be spawned or panics, it releases admission before repeating
/// the complete input on the calling thread.
pub(super) fn run_grouped_aggregate<P, E, C, R>(
    matching_rows: &[usize],
    parallelism: GlobalAggregateParallelism,
    worker_name_prefix: &str,
    chunk: C,
    reduce: R,
) -> Result<P, E>
where
    P: Send,
    E: Send,
    C: Fn(&[usize]) -> Result<P, E> + Sync,
    R: Fn(Vec<P>) -> Result<P, E>,
{
    let Some(admission) = parallelism.try_admit(matching_rows.len()) else {
        return chunk(matching_rows);
    };

    let helper_threads = admission.helper_threads();
    debug_assert!(helper_threads > 0);
    let worker_count = helper_threads.saturating_add(1);
    let chunk = &chunk;
    let parallel_result = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(helper_threads);
        let mut worker_failed = false;
        for chunk_index in 1..worker_count {
            let rows = parallel_aggregate_partition(matching_rows, worker_count, chunk_index);
            let spawn = std::thread::Builder::new()
                .name(format!("{worker_name_prefix}-{chunk_index}"))
                .spawn_scoped(scope, move || chunk(rows));
            match spawn {
                Ok(handle) => handles.push(handle),
                Err(_) => {
                    worker_failed = true;
                    break;
                }
            }
        }

        let mut partials = Vec::with_capacity(worker_count);
        partials.push(chunk(parallel_aggregate_partition(
            matching_rows,
            worker_count,
            0,
        )));
        for handle in handles {
            match handle.join() {
                Ok(partial) => partials.push(partial),
                Err(_) => worker_failed = true,
            }
        }
        if worker_failed {
            return None;
        }

        Some(
            partials
                .into_iter()
                .collect::<Result<Vec<_>, _>>()
                .and_then(reduce),
        )
    });
    drop(admission);
    parallel_result.unwrap_or_else(|| chunk(matching_rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn worker_selection_stays_sequential_through_the_row_threshold() {
        let boundary = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD;
        assert_eq!(
            GlobalAggregateParallelism::fixed(4).worker_count(boundary),
            1
        );
        assert_eq!(
            GlobalAggregateParallelism::fixed(4).worker_count(boundary + 1),
            3
        );
    }

    #[test]
    fn worker_selection_honors_usefulness_database_request_and_hard_caps() {
        assert_eq!(
            GlobalAggregateParallelism::fixed(1).worker_count(usize::MAX),
            1
        );
        assert_eq!(
            GlobalAggregateParallelism::fixed(2).worker_count(usize::MAX),
            2
        );

        let enough_rows_for_every_worker = GLOBAL_AGGREGATE_PARALLEL_ROWS_PER_WORKER
            .saturating_mul(MAX_GLOBAL_AGGREGATE_PARALLEL_WORKERS + 1);
        assert_eq!(
            GlobalAggregateParallelism::fixed(MAX_GLOBAL_AGGREGATE_PARALLEL_WORKERS + 1)
                .worker_count(enough_rows_for_every_worker),
            MAX_GLOBAL_AGGREGATE_PARALLEL_WORKERS
        );

        let configured = GlobalAggregateParallelism::fixed(4);
        assert_eq!(
            configured
                .with_request_worker_cap(2)
                .worker_count(usize::MAX),
            2
        );
        assert_eq!(
            configured
                .with_request_worker_cap(0)
                .worker_count(usize::MAX),
            4
        );
    }

    #[test]
    fn dropping_admission_releases_every_permit() {
        static BUDGET: TestGlobalAggregateWorkerBudget =
            TestGlobalAggregateWorkerBudget::for_test(3);
        let parallelism =
            GlobalAggregateParallelism::budgeted(NonZeroUsize::new(4).unwrap(), &BUDGET);

        let admission = parallelism
            .try_admit(GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1)
            .expect("rows above the threshold acquire helpers");
        assert_eq!(admission.helper_threads(), 2);
        assert_eq!(BUDGET.helpers_in_use(), 2);
        drop(admission);
        assert_eq!(BUDGET.helpers_in_use(), 0);
    }

    #[test]
    fn exhausted_admission_returns_none_without_exceeding_the_budget() {
        static BUDGET: TestGlobalAggregateWorkerBudget =
            TestGlobalAggregateWorkerBudget::for_test(2);
        let parallelism =
            GlobalAggregateParallelism::budgeted(NonZeroUsize::new(4).unwrap(), &BUDGET);
        let held = BUDGET
            .acquire_for_test(2)
            .expect("test saturates the budget");

        assert!(
            parallelism
                .try_admit(GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1)
                .is_none()
        );
        assert_eq!(BUDGET.helpers_in_use(), 2);
        drop(held);
        assert_eq!(BUDGET.helpers_in_use(), 0);
    }

    #[test]
    fn partitions_are_contiguous_balanced_and_deterministic() {
        let rows = [2, 4, 6, 8, 10, 12, 14];
        assert_eq!(parallel_aggregate_partition(&rows, 3, 0), [2, 4, 6]);
        assert_eq!(parallel_aggregate_partition(&rows, 3, 1), [8, 10]);
        assert_eq!(parallel_aggregate_partition(&rows, 3, 2), [12, 14]);

        let short_rows = [3, 5];
        assert_eq!(parallel_aggregate_partition(&short_rows, 3, 0), [3]);
        assert_eq!(parallel_aggregate_partition(&short_rows, 3, 1), [5]);
        assert!(parallel_aggregate_partition(&short_rows, 3, 2).is_empty());
    }

    #[test]
    fn grouped_driver_collects_partitions_in_order_and_preserves_worker_names() {
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let rows = (0..row_count).collect::<Vec<_>>();

        let partials = run_grouped_aggregate(
            &rows,
            GlobalAggregateParallelism::fixed(3),
            "rusthouse-group-bool-count",
            |partition| {
                Ok::<_, ()>(vec![(
                    partition[0],
                    partition.len(),
                    std::thread::current().name().map(str::to_owned),
                )])
            },
            |partials| Ok(partials.into_iter().flatten().collect::<Vec<_>>()),
        )
        .expect("grouped driver succeeds");

        assert_eq!(partials.len(), 3);
        for (worker_index, partial) in partials.iter().enumerate() {
            let expected = parallel_aggregate_partition(&rows, 3, worker_index);
            assert_eq!((partial.0, partial.1), (expected[0], expected.len()));
        }
        assert_eq!(
            partials[1].2.as_deref(),
            Some("rusthouse-group-bool-count-1")
        );
        assert_eq!(
            partials[2].2.as_deref(),
            Some("rusthouse-group-bool-count-2")
        );
    }

    #[test]
    fn grouped_driver_releases_admission_before_complete_input_panic_fallback() {
        static BUDGET: TestGlobalAggregateWorkerBudget =
            TestGlobalAggregateWorkerBudget::for_test(2);
        let row_count = GLOBAL_AGGREGATE_PARALLEL_ROW_THRESHOLD + 1;
        let rows = (0..row_count).collect::<Vec<_>>();
        let used_fallback = AtomicBool::new(false);
        let parallelism =
            GlobalAggregateParallelism::budgeted(NonZeroUsize::new(3).unwrap(), &BUDGET);

        let result = run_grouped_aggregate(
            &rows,
            parallelism,
            "rusthouse-group-bool-test-panic",
            |partition| {
                if partition.len() == row_count {
                    assert_eq!(BUDGET.helpers_in_use(), 0);
                    used_fallback.store(true, Ordering::Release);
                    return Ok::<_, ()>(partition.len());
                }
                if partition[0] > 0 {
                    panic!("injected grouped worker failure");
                }
                Ok(partition.len())
            },
            |_| panic!("worker failure must discard every partial"),
        )
        .expect("complete-input fallback succeeds");

        assert_eq!(result, row_count);
        assert!(used_fallback.load(Ordering::Acquire));
        assert_eq!(BUDGET.helpers_in_use(), 0);
    }
}
