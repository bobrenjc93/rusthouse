//! RustHouse is an experimental, compact analytical database.

pub mod parser;

pub use parser::{
    ColumnDefinition, ColumnType, CreateTable, Keyword, MAX_COLUMNS, MAX_INPUT_BYTES, MAX_TOKENS,
    ParseError, ParseErrorKind, parse_create_table,
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
