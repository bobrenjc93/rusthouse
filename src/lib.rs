#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(
    rust_2018_idioms,
    unreachable_pub,
    clippy::dbg_macro,
    clippy::todo,
    clippy::unimplemented
)]
#![cfg_attr(
    not(test),
    deny(clippy::expect_used, clippy::panic, clippy::unwrap_used)
)]

//! RustHouse is an experimental, compact analytical database.
//!
//! Its storage layer provides validated, in-memory columnar tables with four
//! physical data types. Rows and row batches are checked in full before they
//! are appended, so rejected inserts never leave columns with different
//! lengths.

mod error;
mod storage;
mod value;

pub mod csv;
pub use csv::{CsvError, CsvFormatter, CsvLimits, CsvRecord};
pub use error::{Error, Result};
pub use storage::{Column, ColumnDef, StorageLimits, Table};
pub use value::{DataType, Value};

pub mod sql;

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
