//! RustHouse is an experimental, compact analytical database.

pub mod scan;

pub use scan::{ComparisonOperator, ScanError, ScanLimits, scan_nullable_i64};

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
