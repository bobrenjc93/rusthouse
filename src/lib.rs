//! RustHouse is an experimental, compact analytical database.

pub mod sql;

pub use sql::{
    ColumnDefinition, CreateTableStatement, DataType, IdentifierContext, ParseError,
    ParseErrorKind, ParseLimits, parse_create_table, parse_create_table_with_limits,
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
