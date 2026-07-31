//! Configurable execution budgets and accounting.

use std::mem::size_of;

use crate::error::{Error, Resource, Result};
use crate::value::Value;

/// Resource ceilings applied to one [`Database::execute`](crate::Database::execute) batch.
///
/// `max_stored_values` is the one persistent ceiling: it applies to the total
/// number of values stored in the database after an insert. `max_memory_bytes`
/// accounts for transient executor-owned buffers and returned values; table
/// storage and parser allocations are bounded separately by their specific
/// limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionLimits {
    /// Maximum UTF-8 bytes accepted in one SQL batch.
    pub max_input_bytes: usize,
    /// Maximum lexical tokens accepted in one SQL batch.
    pub max_tokens: usize,
    /// Maximum statements accepted in one SQL batch.
    pub max_statements: usize,
    /// Maximum columns accepted in one table schema.
    pub max_schema_width: usize,
    /// Maximum values stored across all tables.
    pub max_stored_values: usize,
    /// Maximum rows produced between execution operators in one batch.
    pub max_intermediate_rows: usize,
    /// Maximum estimated bytes in transient execution buffers and results.
    pub max_memory_bytes: usize,
    /// Maximum result rows returned by all queries in one batch.
    pub max_result_rows: usize,
    /// Maximum bytes emitted by one bounded render operation.
    pub max_rendered_bytes: usize,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 16 * 1024 * 1024,
            max_tokens: 1_000_000,
            max_statements: 10_000,
            max_schema_width: 65_536,
            max_stored_values: 100_000_000,
            max_intermediate_rows: 10_000_000,
            max_memory_bytes: 64 * 1024 * 1024,
            max_result_rows: 1_000_000,
            max_rendered_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Resource counters from the most recent execution attempt.
///
/// Counters remain available after an error, making limit failures observable
/// without parsing error text.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExecutionStats {
    /// SQL input bytes supplied to the batch.
    pub input_bytes: usize,
    /// Non-EOF tokens parsed from the batch.
    pub tokens: usize,
    /// Statements parsed from the batch.
    pub statements: usize,
    /// Widest schema encountered while parsing or executing the batch.
    pub schema_width: usize,
    /// Values stored across all tables after the execution attempt.
    pub stored_values: usize,
    /// Rows emitted between operators during the batch.
    pub intermediate_rows: usize,
    /// Highest estimated transient allocation during the batch.
    pub peak_memory_bytes: usize,
    /// Rows returned by queries in the batch.
    pub result_rows: usize,
    /// Bytes written to temporary spill runs, including merge passes.
    pub spilled_bytes: usize,
    /// Number of temporary spill runs created.
    pub spill_runs: usize,
    /// Highest number of temporary spill files live at once.
    pub peak_live_spill_runs: usize,
}

#[derive(Debug)]
pub(crate) struct ExecutionContext<'a> {
    limits: &'a ExecutionLimits,
    pub(crate) stats: ExecutionStats,
    memory_bytes: usize,
}

impl<'a> ExecutionContext<'a> {
    pub(crate) fn new(limits: &'a ExecutionLimits, input_bytes: usize) -> Self {
        Self {
            limits,
            stats: ExecutionStats {
                input_bytes,
                ..ExecutionStats::default()
            },
            memory_bytes: 0,
        }
    }

    pub(crate) fn available_memory(&self) -> usize {
        self.limits
            .max_memory_bytes
            .saturating_sub(self.memory_bytes)
    }

    pub(crate) fn check(&self, resource: Resource, actual: usize) -> Result<()> {
        let limit = match resource {
            Resource::InputBytes => self.limits.max_input_bytes,
            Resource::Tokens => self.limits.max_tokens,
            Resource::Statements => self.limits.max_statements,
            Resource::SchemaWidth => self.limits.max_schema_width,
            Resource::StoredValues => self.limits.max_stored_values,
            Resource::IntermediateRows => self.limits.max_intermediate_rows,
            Resource::MemoryBytes => self.limits.max_memory_bytes,
            Resource::ResultRows => self.limits.max_result_rows,
            Resource::RenderedBytes => self.limits.max_rendered_bytes,
        };
        if actual > limit {
            return Err(Error::ResourceLimitExceeded {
                resource,
                limit,
                actual,
            });
        }
        Ok(())
    }

    pub(crate) fn add_intermediate_rows(&mut self, count: usize) -> Result<()> {
        let actual = self.stats.intermediate_rows.saturating_add(count);
        self.check(Resource::IntermediateRows, actual)?;
        self.stats.intermediate_rows = actual;
        Ok(())
    }

    pub(crate) fn add_result_row(&mut self) -> Result<()> {
        let rows = self.stats.result_rows.saturating_add(1);
        self.check(Resource::ResultRows, rows)?;
        self.stats.result_rows = rows;
        Ok(())
    }

    pub(crate) fn reserve_memory(&mut self, bytes: usize) -> Result<()> {
        let actual = self.memory_bytes.saturating_add(bytes);
        self.check(Resource::MemoryBytes, actual)?;
        self.memory_bytes = actual;
        self.stats.peak_memory_bytes = self.stats.peak_memory_bytes.max(actual);
        Ok(())
    }

    pub(crate) fn release_memory(&mut self, bytes: usize) {
        self.memory_bytes = self.memory_bytes.saturating_sub(bytes);
    }

    pub(crate) fn adjust_memory_reservation(
        &mut self,
        reserved: usize,
        actual: usize,
    ) -> Result<()> {
        if actual > reserved {
            self.reserve_memory(actual - reserved)
        } else {
            self.release_memory(reserved - actual);
            Ok(())
        }
    }

    pub(crate) fn record_spill(&mut self, bytes: usize) {
        self.stats.spilled_bytes = self.stats.spilled_bytes.saturating_add(bytes);
        self.stats.spill_runs = self.stats.spill_runs.saturating_add(1);
    }

    pub(crate) fn observe_live_spill_runs(&mut self, count: usize) {
        self.stats.peak_live_spill_runs = self.stats.peak_live_spill_runs.max(count);
    }
}

pub(crate) fn estimated_value_bytes(value: &Value) -> usize {
    size_of::<Value>()
        + match value {
            Value::String(value) => value.len(),
            Value::Int64(_) | Value::Float64(_) | Value::Bool(_) => 0,
        }
}

pub(crate) fn estimated_row_bytes(row: &[Value]) -> usize {
    size_of::<Vec<Value>>() + row.iter().map(estimated_value_bytes).sum::<usize>()
}
