use super::lexer::Parser;
use super::limits::enforce_sql_size;
use super::{ParseError, SelectParseLimits, SelectStatement};

/// Parses one bounded `SELECT * FROM <table>` statement using the default
/// limits.
///
/// Keywords are ASCII case-insensitive. Identifiers consist of an ASCII
/// letter or underscore followed by ASCII letters, digits, or underscores.
/// One optional trailing semicolon is accepted.
///
/// # Errors
///
/// Returns [`ParseError`] when the input exceeds the default resource limit or
/// does not match the supported `SELECT * FROM <table>` grammar.
///
/// # Examples
///
/// ```
/// use rusthouse::sql::parse_select;
///
/// let statement = parse_select("SELECT * FROM readings;")?;
/// assert_eq!(statement.table_name, "readings");
/// # Ok::<(), rusthouse::sql::ParseError>(())
/// ```
pub fn parse_select(sql: &str) -> Result<SelectStatement, ParseError> {
    parse_select_with_limits(sql, SelectParseLimits::default())
}

/// Parses one `SELECT * FROM <table>` statement using caller-provided
/// resource limits.
///
/// # Errors
///
/// Returns [`ParseError`] when the input exceeds `limits` or does not match the
/// supported `SELECT * FROM <table>` grammar.
pub fn parse_select_with_limits(
    sql: &str,
    limits: SelectParseLimits,
) -> Result<SelectStatement, ParseError> {
    enforce_sql_size(sql, limits.max_sql_bytes)?;

    let mut parser = Parser::new(sql);
    parser.expect_keyword("SELECT")?;
    parser.expect_text("*", "'*'")?;
    parser.expect_keyword("FROM")?;
    let table_name = parser.expect_identifier("table name")?;
    parser.finish_statement()?;

    Ok(SelectStatement { table_name })
}
