//! RustHouse is a compact, dependency-free, in-memory analytical database.
//!
//! [`Engine`] executes semicolon-delimited SQL scripts while retaining its
//! catalog across calls. Query results can be encoded with [`render`].

mod engine;
mod error;
mod format;
mod sql;
mod storage;

pub use engine::{Engine, QueryResult};
pub use error::Error;
pub use format::{OutputFormat, render};
pub use storage::{ColumnSchema, DataType, Value};

/// Returns the product name.
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

    #[test]
    fn executes_an_end_to_end_sql_script() {
        let mut engine = Engine::new();
        let results = engine
            .execute(
                "CREATE TABLE t (id Int64, label String);\
                 INSERT INTO t VALUES (1, 'one'), (2, 'two');\
                 SELECT label FROM t WHERE id > 1;",
            )
            .unwrap();
        assert_eq!(results[0].rows, vec![vec![Value::String("two".to_owned())]]);
    }
}
