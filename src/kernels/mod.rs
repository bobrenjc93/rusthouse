//! Selection-aware execution kernels over [`crate::batch::RecordBatch`].

mod aggregate;
mod group;
mod predicate;

pub use aggregate::{
    AggregateExpr, AggregateKind, AggregateResult, ScalarValue, SumValue, aggregate, avg, count,
    max, min, sum,
};
pub use group::{GroupByConfig, GroupKey, GroupView, GroupedResults, hash_group};
pub use predicate::{
    ComparisonOp, compare_bool, compare_columns, compare_f64, compare_i64, compare_string,
    is_not_null, is_null,
};
