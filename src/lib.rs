//! RustHouse is an experimental, compact analytical database.
#![warn(missing_docs)]

pub mod http;
pub mod query;

pub use query::{
    QueryCancellation, QueryError, QueryErrorKind, QueryFuture, QueryRequest, QueryResult,
    QueryService, QueryValue, ServiceHealth,
};

/// Returns the product name while the first storage engine is being built.
pub fn product_name() -> &'static str {
    "RustHouse"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_the_database() {
        assert_eq!(product_name(), "RustHouse");
    }
}
