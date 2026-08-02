//! RustHouse is an experimental, compact analytical database.

use std::fmt::{self, Write as _};

mod table;

pub use table::{Column, DataType, Field, Schema, SchemaError, Table, TableError, Value};

/// Maximum number of SQL bytes accepted from either the command line or stdin.
pub const MAX_SQL_INPUT_BYTES: usize = 1024 * 1024;

const EXPECTED_QUERY: &str =
    "expected SELECT <signed Int64> [AS <identifier>] with an optional trailing semicolon";

/// An error produced while parsing the currently supported SQL subset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlError {
    /// The statement is outside the literal `SELECT` grammar.
    Unsupported,
    /// The literal has integer syntax but is outside the `Int64` range.
    IntegerOutOfRange,
    /// The alias is not an unquoted SQL identifier.
    InvalidAlias(String),
}

impl fmt::Display for SqlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => write!(formatter, "unsupported SQL: {EXPECTED_QUERY}"),
            Self::IntegerOutOfRange => {
                formatter.write_str("integer literal is outside the signed Int64 range")
            }
            Self::InvalidAlias(alias) => write!(
                formatter,
                "invalid column alias '{alias}': expected an ASCII letter or underscore, followed by ASCII letters, digits, or underscores"
            ),
        }
    }
}

impl std::error::Error for SqlError {}

/// Executes the supported literal `SELECT` grammar and returns CSV with a header.
///
/// The grammar is `SELECT <signed Int64> [AS <identifier>] [;]`. Keywords are
/// case-insensitive. The unaliased column name is the literal as written.
pub fn execute_literal_select_csv(sql: &str) -> Result<String, SqlError> {
    let table = execute_literal_select(sql)?;
    Ok(table_to_csv(&table))
}

fn execute_literal_select(sql: &str) -> Result<Table, SqlError> {
    let query = parse_literal_select(sql)?;
    let schema = Schema::new(vec![Field::new(query.column_name, DataType::Int64)])
        .expect("a single-field query result always has a valid schema");
    let mut table = Table::new(schema);
    table
        .append_row(vec![Value::Int64(query.value)])
        .expect("the literal value matches the query result schema");
    Ok(table)
}

fn table_to_csv(table: &Table) -> String {
    let mut csv = String::new();

    for (index, field) in table.schema().fields().iter().enumerate() {
        if index > 0 {
            csv.push(',');
        }
        push_csv_field(&mut csv, &field.name);
    }
    csv.push('\n');

    for row in 0..table.row_count() {
        for (index, column) in table.columns().iter().enumerate() {
            if index > 0 {
                csv.push(',');
            }
            push_csv_value(&mut csv, column, row);
        }
        csv.push('\n');
    }

    csv
}

fn push_csv_value(csv: &mut String, column: &Column, row: usize) {
    match column {
        Column::Int64(values) => write!(csv, "{}", values[row]),
        Column::Float64(values) => write!(csv, "{}", values[row]),
        Column::Bool(values) => write!(csv, "{}", values[row]),
        Column::String(values) => {
            push_csv_field(csv, &values[row]);
            return;
        }
    }
    .expect("writing to a String cannot fail");
}

struct LiteralSelect {
    value: i64,
    column_name: String,
}

fn parse_literal_select(sql: &str) -> Result<LiteralSelect, SqlError> {
    let statement = sql.trim();
    let statement = statement
        .strip_suffix(';')
        .map_or(statement, |without_semicolon| without_semicolon.trim_end());
    let tokens: Vec<_> = statement.split_whitespace().collect();

    if tokens.len() != 2 && tokens.len() != 4 {
        return Err(SqlError::Unsupported);
    }
    if !tokens[0].eq_ignore_ascii_case("SELECT") {
        return Err(SqlError::Unsupported);
    }

    let literal = tokens[1];
    if !has_signed_integer_syntax(literal) {
        return Err(SqlError::Unsupported);
    }
    let value = literal
        .parse::<i64>()
        .map_err(|_| SqlError::IntegerOutOfRange)?;

    let column_name = if tokens.len() == 4 {
        if !tokens[2].eq_ignore_ascii_case("AS") {
            return Err(SqlError::Unsupported);
        }
        validate_alias(tokens[3])?;
        tokens[3]
    } else {
        literal
    };

    Ok(LiteralSelect {
        value,
        column_name: column_name.to_owned(),
    })
}

fn has_signed_integer_syntax(literal: &str) -> bool {
    let digits = literal
        .strip_prefix(['+', '-'])
        .unwrap_or(literal)
        .as_bytes();
    !digits.is_empty() && digits.iter().all(u8::is_ascii_digit)
}

fn validate_alias(alias: &str) -> Result<(), SqlError> {
    let mut bytes = alias.bytes();
    let Some(first) = bytes.next() else {
        return Err(SqlError::InvalidAlias(alias.to_owned()));
    };
    if !(first.is_ascii_alphabetic() || first == b'_')
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(SqlError::InvalidAlias(alias.to_owned()));
    }
    Ok(())
}

fn push_csv_field(csv: &mut String, field: &str) {
    if field
        .bytes()
        .any(|byte| matches!(byte, b',' | b'"' | b'\n' | b'\r'))
    {
        csv.push('"');
        for character in field.chars() {
            if character == '"' {
                csv.push('"');
            }
            csv.push(character);
        }
        csv.push('"');
    } else {
        csv.push_str(field);
    }
}

/// Returns the product name.
pub fn product_name() -> &'static str {
    "RustHouse"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executes_int64_boundaries() {
        assert_eq!(
            execute_literal_select_csv("select -9223372036854775808 AS minimum;").unwrap(),
            "minimum\n-9223372036854775808\n"
        );
        assert_eq!(
            execute_literal_select_csv("SELECT +9223372036854775807").unwrap(),
            "+9223372036854775807\n9223372036854775807\n"
        );
    }

    #[test]
    fn executes_into_a_schema_checked_columnar_result() {
        let table = execute_literal_select("SELECT -42 AS answer").unwrap();

        assert_eq!(
            table.schema().fields(),
            &[Field::new("answer", DataType::Int64)]
        );
        assert_eq!(table.columns(), &[Column::Int64(vec![-42])]);
        assert_eq!(table.row_count(), 1);
    }

    #[test]
    fn distinguishes_overflow_from_unsupported_syntax() {
        assert_eq!(
            execute_literal_select_csv("SELECT 9223372036854775808"),
            Err(SqlError::IntegerOutOfRange)
        );
        assert_eq!(
            execute_literal_select_csv("SELECT 1 + 2"),
            Err(SqlError::Unsupported)
        );
    }

    #[test]
    fn rejects_invalid_aliases() {
        assert_eq!(
            execute_literal_select_csv("SELECT 1 AS 2columns"),
            Err(SqlError::InvalidAlias("2columns".to_owned()))
        );
    }

    #[test]
    fn identifies_the_database() {
        assert_eq!(product_name(), "RustHouse");
    }
}
