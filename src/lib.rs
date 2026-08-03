//! RustHouse is an experimental, compact analytical database.

mod storage;

pub use storage::{ColumnSchema, DataType, InsertError, Int64Table, Schema};

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
