use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// Resource limits applied to all SELECT statements in one execution batch.
///
/// A missing limit is unlimited. Row limits are inclusive: a maximum of `10`
/// permits exactly ten rows and rejects an attempt to scan or emit row eleven.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecutionLimits {
    pub max_scan_rows: Option<usize>,
    pub max_output_rows: Option<usize>,
    pub deadline: Option<Instant>,
}

impl ExecutionLimits {
    /// Construct an explicitly unlimited set of execution limits.
    #[must_use]
    pub const fn unlimited() -> Self {
        Self {
            max_scan_rows: None,
            max_output_rows: None,
            deadline: None,
        }
    }
}

/// A thread-safe, cloneable signal for cooperatively cancelling execution.
#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation. All clones observe the same one-way signal.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// Controls one call to [`crate::Database::execute_with_options`].
#[derive(Debug, Clone, Default)]
pub struct ExecutionOptions {
    pub limits: ExecutionLimits,
    pub cancellation_token: CancellationToken,
}

impl ExecutionOptions {
    #[must_use]
    pub const fn new(limits: ExecutionLimits, cancellation_token: CancellationToken) -> Self {
        Self {
            limits,
            cancellation_token,
        }
    }
}

impl From<ExecutionLimits> for ExecutionOptions {
    fn from(limits: ExecutionLimits) -> Self {
        Self {
            limits,
            cancellation_token: CancellationToken::new(),
        }
    }
}

impl From<&ExecutionOptions> for ExecutionOptions {
    fn from(options: &ExecutionOptions) -> Self {
        options.clone()
    }
}
