//! RustHouse is an experimental, compact analytical database.

mod format;
mod parser;

pub use format::render_csv;
pub use parser::{SelectStatement, SqlError, parse_sql};

/// Maximum number of bytes accepted at the SQL input boundary.
pub const MAX_INPUT_BYTES: usize = 64 * 1024;

/// Returns the product name.
pub fn product_name() -> &'static str {
    "RustHouse"
}

/// Parses supported SQL and returns its results in CSV-with-header format.
pub fn execute_sql(input: &str) -> Result<String, SqlError> {
    parse_sql(input).map(|statements| render_csv(&statements))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_the_database() {
        assert_eq!(product_name(), "RustHouse");
    }
}
