use crate::{DataType, Value};

/// One named, typed column in a `CREATE TABLE` statement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ColumnDefinition {
    /// The column name exactly as it appeared in the statement.
    pub name: String,
    /// The parsed type of the column.
    pub data_type: DataType,
}

/// The typed result of parsing one `CREATE TABLE` statement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateTableStatement {
    /// The table name exactly as it appeared in the statement.
    pub table_name: String,
    /// Columns in the order in which they appeared in the statement.
    pub columns: Vec<ColumnDefinition>,
}

/// The typed result of parsing one `INSERT INTO ... VALUES` statement.
///
/// `rows` has the same representation accepted by
/// [`crate::Table::insert_batch`].
#[derive(Clone, Debug, PartialEq)]
pub struct InsertStatement {
    /// The target table name exactly as it appeared in the statement.
    pub table_name: String,
    /// Rows and their values in statement order.
    pub rows: Vec<Vec<Value>>,
}

/// The typed result of parsing one `SELECT * FROM ...` statement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectStatement {
    /// The source table name exactly as it appeared in the statement.
    pub table_name: String,
}
