use std::error::Error;
use std::fmt;

use crate::MAX_INPUT_BYTES;

/// A parsed literal `SELECT` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectStatement {
    value: i64,
    alias: Option<String>,
}

impl SelectStatement {
    /// Returns the selected integer value.
    pub fn value(&self) -> i64 {
        self.value
    }

    /// Returns the explicit result-column alias, when one was supplied.
    pub fn alias(&self) -> Option<&str> {
        self.alias.as_deref()
    }

    /// Returns the result-column name used by output formats.
    pub fn column_name(&self) -> &str {
        self.alias.as_deref().unwrap_or("value")
    }
}

/// An error at the deliberately narrow SQL parsing boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlError {
    /// The input exceeds [`MAX_INPUT_BYTES`].
    InputTooLarge,
    /// No SQL statement was provided.
    NoStatements,
    /// A separator was not followed by another statement.
    EmptyStatement { statement: usize },
    /// The statement did not start with `SELECT`.
    UnsupportedStatement { statement: usize },
    /// The `SELECT` did not contain a literal.
    MissingLiteral { statement: usize },
    /// The selected token was not a signed decimal integer literal.
    InvalidLiteral { statement: usize, literal: String },
    /// The literal was outside the `Int64` range.
    IntegerOverflow { statement: usize, literal: String },
    /// A token other than `AS` followed the literal.
    ExpectedAs { statement: usize, token: String },
    /// `AS` was not followed by an identifier.
    MissingAlias { statement: usize },
    /// An alias was not an unquoted ASCII SQL identifier.
    InvalidIdentifier { statement: usize, alias: String },
    /// More tokens followed a complete literal `SELECT`.
    UnexpectedToken { statement: usize, token: String },
}

impl fmt::Display for SqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputTooLarge => write!(f, "SQL input exceeds the {MAX_INPUT_BYTES}-byte limit"),
            Self::NoStatements => write!(f, "no SQL statements were provided"),
            Self::EmptyStatement { statement } => {
                write!(f, "statement {statement} is empty")
            }
            Self::UnsupportedStatement { statement } => {
                write!(f, "statement {statement}: unsupported SQL; expected SELECT")
            }
            Self::MissingLiteral { statement } => write!(
                f,
                "statement {statement}: expected a signed Int64 literal after SELECT"
            ),
            Self::InvalidLiteral { statement, literal } => write!(
                f,
                "statement {statement}: expected a signed Int64 literal, found {literal:?}"
            ),
            Self::IntegerOverflow { statement, literal } => write!(
                f,
                "statement {statement}: integer literal {literal:?} is outside the Int64 range"
            ),
            Self::ExpectedAs { statement, token } => write!(
                f,
                "statement {statement}: expected AS after the literal, found {token:?}"
            ),
            Self::MissingAlias { statement } => {
                write!(f, "statement {statement}: expected an identifier after AS")
            }
            Self::InvalidIdentifier { statement, alias } => {
                write!(f, "statement {statement}: invalid identifier {alias:?}")
            }
            Self::UnexpectedToken { statement, token } => write!(
                f,
                "statement {statement}: unexpected token {token:?} after the alias"
            ),
        }
    }
}

impl Error for SqlError {}

/// Parses semicolon-separated `SELECT <signed Int64> [AS identifier]` statements.
pub fn parse_sql(input: &str) -> Result<Vec<SelectStatement>, SqlError> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(SqlError::InputTooLarge);
    }

    let input = input.trim();
    if input.is_empty() {
        return Err(SqlError::NoStatements);
    }

    // Exactly one trailing semicolon is allowed without creating an empty statement.
    let statements = input.strip_suffix(';').unwrap_or(input);
    if statements.trim().is_empty() {
        return Err(SqlError::NoStatements);
    }

    statements
        .split(';')
        .enumerate()
        .map(|(index, statement)| parse_statement(statement, index + 1))
        .collect()
}

fn parse_statement(statement: &str, statement_number: usize) -> Result<SelectStatement, SqlError> {
    let mut tokens = statement.split_whitespace();
    let Some(select) = tokens.next() else {
        return Err(SqlError::EmptyStatement {
            statement: statement_number,
        });
    };

    if !select.eq_ignore_ascii_case("SELECT") {
        return Err(SqlError::UnsupportedStatement {
            statement: statement_number,
        });
    }

    let literal = tokens.next().ok_or(SqlError::MissingLiteral {
        statement: statement_number,
    })?;
    if !is_integer_literal(literal) {
        return Err(SqlError::InvalidLiteral {
            statement: statement_number,
            literal: literal.to_owned(),
        });
    }

    let value = literal
        .parse::<i64>()
        .map_err(|_| SqlError::IntegerOverflow {
            statement: statement_number,
            literal: literal.to_owned(),
        })?;

    let alias = match tokens.next() {
        None => None,
        Some(token) if token.eq_ignore_ascii_case("AS") => {
            let alias = tokens.next().ok_or(SqlError::MissingAlias {
                statement: statement_number,
            })?;
            if !is_identifier(alias) {
                return Err(SqlError::InvalidIdentifier {
                    statement: statement_number,
                    alias: alias.to_owned(),
                });
            }
            if let Some(token) = tokens.next() {
                return Err(SqlError::UnexpectedToken {
                    statement: statement_number,
                    token: token.to_owned(),
                });
            }
            Some(alias.to_owned())
        }
        Some(token) => {
            return Err(SqlError::ExpectedAs {
                statement: statement_number,
                token: token.to_owned(),
            });
        }
    };

    Ok(SelectStatement { value, alias })
}

fn is_integer_literal(token: &str) -> bool {
    let digits = token.strip_prefix(['+', '-']).unwrap_or(token).as_bytes();
    !digits.is_empty() && digits.iter().all(u8::is_ascii_digit)
}

fn is_identifier(token: &str) -> bool {
    let mut bytes = token.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'A'..=b'Z' | b'_'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_int64_boundaries_and_case_insensitive_keywords() {
        let parsed =
            parse_sql("select -9223372036854775808 as minimum; SeLeCt +9223372036854775807")
                .unwrap();

        assert_eq!(parsed[0].value(), i64::MIN);
        assert_eq!(parsed[0].column_name(), "minimum");
        assert_eq!(parsed[1].value(), i64::MAX);
        assert_eq!(parsed[1].column_name(), "value");
    }

    #[test]
    fn rejects_empty_statements_between_separators() {
        assert_eq!(
            parse_sql("SELECT 1;;"),
            Err(SqlError::EmptyStatement { statement: 2 })
        );
    }

    #[test]
    fn rejects_non_identifiers() {
        assert_eq!(
            parse_sql("SELECT 1 AS two-words"),
            Err(SqlError::InvalidIdentifier {
                statement: 1,
                alias: "two-words".to_owned(),
            })
        );
    }
}
