//! SQL syntax handling.

pub mod insert;
pub mod lexer;

pub use insert::{
    DEFAULT_MAX_INSERT_ROWS, DEFAULT_MAX_INSERT_VALUES, InsertError, InsertLimits, execute_insert,
    execute_insert_with_limits,
};
