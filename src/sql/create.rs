use crate::DataType;

use super::lexer::{Parser, TokenKind};
use super::limits::enforce_sql_size;
use super::{ColumnDefinition, CreateTableStatement, ParseError, ParseLimits};

/// Parses one bounded `CREATE TABLE` statement using the default limits.
///
/// Keywords and the four supported data types are ASCII case-insensitive.
/// Identifiers consist of an ASCII letter or underscore followed by ASCII
/// letters, digits, or underscores. One optional trailing semicolon is
/// accepted.
///
/// # Errors
///
/// Returns [`ParseError`] when the input exceeds the default resource limits
/// or does not match the supported `CREATE TABLE` grammar.
///
/// # Examples
///
/// ```
/// use rusthouse::sql::{DataType, parse_create_table};
///
/// let statement = parse_create_table(
///     "CREATE TABLE readings (time Int64, value Float64, valid Bool, tag String);",
/// )?;
/// assert_eq!(statement.table_name, "readings");
/// assert_eq!(statement.columns[1].data_type, DataType::Float64);
/// # Ok::<(), rusthouse::sql::ParseError>(())
/// ```
pub fn parse_create_table(sql: &str) -> Result<CreateTableStatement, ParseError> {
    parse_create_table_with_limits(sql, ParseLimits::default())
}

/// Parses one `CREATE TABLE` statement using caller-provided resource limits.
///
/// # Errors
///
/// Returns [`ParseError`] when the input exceeds `limits` or does not match the
/// supported `CREATE TABLE` grammar.
pub fn parse_create_table_with_limits(
    sql: &str,
    limits: ParseLimits,
) -> Result<CreateTableStatement, ParseError> {
    enforce_sql_size(sql, limits.max_sql_bytes)?;
    parse(sql, limits.max_columns)
}

fn parse(sql: &str, max_columns: usize) -> Result<CreateTableStatement, ParseError> {
    let mut parser = Parser::new(sql);
    parser.expect_keyword("CREATE")?;
    parser.expect_keyword("TABLE")?;
    let table_name = parser.expect_identifier("table name")?;
    parser.expect_kind(TokenKind::LeftParenthesis, "'('")?;

    let mut columns = Vec::with_capacity(max_columns.min(16));
    loop {
        let column_token = parser.expect_word("column name")?;
        if columns.len() == max_columns {
            return Err(ParseError::TooManyColumns {
                position: column_token.start,
                max_columns,
            });
        }

        let column_name = parser.token_text(column_token).to_owned();
        let type_token = parser.expect_word("column type")?;
        let type_name = parser.token_text(type_token);
        let data_type = parse_data_type(type_name).ok_or_else(|| ParseError::UnsupportedType {
            position: type_token.start,
            type_name: type_name.to_owned(),
        })?;
        columns.push(ColumnDefinition {
            name: column_name,
            data_type,
        });

        match parser.peek() {
            Some(token) if token.kind == TokenKind::Comma => {
                parser.next();
            }
            Some(token) if token.kind == TokenKind::RightParenthesis => {
                parser.next();
                break;
            }
            token => return Err(parser.syntax_error("',' or ')'", token)),
        }
    }

    parser.finish_statement()?;
    Ok(CreateTableStatement {
        table_name,
        columns,
    })
}

fn parse_data_type(type_name: &str) -> Option<DataType> {
    if type_name.eq_ignore_ascii_case("Int64") {
        Some(DataType::Int64)
    } else if type_name.eq_ignore_ascii_case("Float64") {
        Some(DataType::Float64)
    } else if type_name.eq_ignore_ascii_case("Bool") {
        Some(DataType::Bool)
    } else if type_name.eq_ignore_ascii_case("String") {
        Some(DataType::String)
    } else {
        None
    }
}
