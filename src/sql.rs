//! Parsing for RustHouse's bounded SQL surface.

mod ast;
mod create;
mod error;
mod insert;
mod lexer;
mod limits;
mod select;

pub use crate::{DataType, Value};
pub use ast::{ColumnDefinition, CreateTableStatement, InsertStatement, SelectStatement};
pub use create::{parse_create_table, parse_create_table_with_limits};
pub use error::ParseError;
pub use insert::{parse_insert, parse_insert_with_limits};
pub use limits::{
    InsertParseLimits, MAX_COLUMNS, MAX_INSERT_ROWS, MAX_INSERT_STRING_BYTES, MAX_INSERT_VALUES,
    MAX_SQL_BYTES, ParseLimits, SelectParseLimits,
};
pub use select::{parse_select, parse_select_with_limits};
