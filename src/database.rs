//! Stateful dispatch for bounded SQL statement sequences.

use std::error::Error;
use std::fmt;

use crate::catalog::Catalog;
use crate::ddl::{CreateTableError, execute_create_table};
use crate::dml::{InsertValuesError, execute_insert_values};
use crate::lexer::{Delimiter, LexError, LexerLimits, Token, TokenKind, lex};
use crate::query::{
    ScalarSelect, ScalarSelectError, TableSelectError, TableSelectResult, execute_table_select,
    parse_scalar_select,
};

/// Maximum number of statements accepted by one call to [`Database::execute`].
pub const MAX_SCRIPT_STATEMENTS: usize = 1024;

/// A result produced by a supported `SELECT` statement.
#[derive(Clone, Debug, PartialEq)]
pub enum SelectResult {
    /// A `SELECT` whose expression is one scalar literal.
    Scalar(ScalarSelect),
    /// A projection from a catalog table.
    Table(TableSelectResult),
}

/// An in-memory database that preserves catalog state between executions.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Database {
    catalog: Catalog,
}

impl Database {
    /// Creates an empty database.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the database's catalog.
    #[must_use]
    pub const fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Executes a bounded semicolon-delimited sequence in source order.
    ///
    /// Only the existing `CREATE TABLE`, one-row `INSERT INTO ... VALUES`,
    /// scalar `SELECT`, and table projection `SELECT` shapes are dispatched.
    /// Command statements produce no result; each `SELECT` contributes one
    /// result in statement order. Statements completed before an execution
    /// error remain applied.
    pub fn execute(&mut self, input: &str) -> Result<Vec<SelectResult>, DatabaseError> {
        let tokens = lex(input, LexerLimits::default()).map_err(DatabaseError::Lex)?;
        let statements = split_statements(input, &tokens)?;
        let mut results = Vec::new();

        for (statement_index, statement) in statements.into_iter().enumerate() {
            let first = statement
                .tokens
                .first()
                .expect("the statement splitter rejects empty statements");
            let TokenKind::Identifier(keyword) = &first.kind else {
                return Err(DatabaseError::UnsupportedStatement {
                    statement_index,
                    position: first.span.start,
                });
            };

            if keyword.eq_ignore_ascii_case("CREATE") {
                execute_create_table(&mut self.catalog, statement.sql).map_err(|source| {
                    DatabaseError::Create {
                        statement_index,
                        source,
                    }
                })?;
            } else if keyword.eq_ignore_ascii_case("INSERT") {
                execute_insert_values(&mut self.catalog, statement.sql).map_err(|source| {
                    DatabaseError::Insert {
                        statement_index,
                        source,
                    }
                })?;
            } else if keyword.eq_ignore_ascii_case("SELECT") {
                if is_table_select(statement.tokens) {
                    let result =
                        execute_table_select(&self.catalog, statement.sql).map_err(|source| {
                            DatabaseError::TableSelect {
                                statement_index,
                                source,
                            }
                        })?;
                    results.push(SelectResult::Table(result));
                } else {
                    let result = parse_scalar_select(statement.sql).map_err(|source| {
                        DatabaseError::ScalarSelect {
                            statement_index,
                            source,
                        }
                    })?;
                    results.push(SelectResult::Scalar(result));
                }
            } else {
                return Err(DatabaseError::UnsupportedStatement {
                    statement_index,
                    position: first.span.start,
                });
            }
        }

        Ok(results)
    }
}

fn is_table_select(tokens: &[Token]) -> bool {
    matches!(
        tokens.get(1).map(|token| &token.kind),
        Some(TokenKind::Identifier(_) | TokenKind::QuotedIdentifier(_))
    )
}

struct Statement<'a> {
    sql: &'a str,
    tokens: &'a [Token],
}

fn split_statements<'a>(
    input: &'a str,
    tokens: &'a [Token],
) -> Result<Vec<Statement<'a>>, DatabaseError> {
    if tokens.is_empty() {
        return Err(DatabaseError::EmptyScript);
    }

    let mut statements = Vec::new();
    let mut statement_start = 0;
    for (index, token) in tokens.iter().enumerate() {
        if token.kind != TokenKind::Delimiter(Delimiter::Semicolon) {
            continue;
        }
        if index == statement_start {
            return Err(DatabaseError::EmptyStatement {
                position: token.span.start,
            });
        }
        push_statement(&mut statements, input, tokens, statement_start, index + 1)?;
        statement_start = index + 1;
    }

    if statement_start < tokens.len() {
        push_statement(
            &mut statements,
            input,
            tokens,
            statement_start,
            tokens.len(),
        )?;
    }

    Ok(statements)
}

fn push_statement<'a>(
    statements: &mut Vec<Statement<'a>>,
    input: &'a str,
    tokens: &'a [Token],
    start: usize,
    end: usize,
) -> Result<(), DatabaseError> {
    if statements.len() == MAX_SCRIPT_STATEMENTS {
        return Err(DatabaseError::StatementLimitExceeded {
            limit: MAX_SCRIPT_STATEMENTS,
        });
    }

    let statement_tokens = &tokens[start..end];
    let source_start = statement_tokens
        .first()
        .expect("statement token range is nonempty")
        .span
        .start;
    let source_end = statement_tokens
        .last()
        .expect("statement token range is nonempty")
        .span
        .end;
    statements.push(Statement {
        sql: &input[source_start..source_end],
        tokens: statement_tokens,
    });
    Ok(())
}

/// An error returned while splitting, dispatching, or executing a script.
#[derive(Clone, Debug, PartialEq)]
pub enum DatabaseError {
    /// Tokenization of the complete script failed.
    Lex(LexError),
    /// The script contains no statement tokens.
    EmptyScript,
    /// Two terminators occur without a statement between them.
    EmptyStatement {
        /// Zero-based byte position of the unexpected terminator.
        position: usize,
    },
    /// The script contains more statements than one execution permits.
    StatementLimitExceeded {
        /// Maximum number of statements accepted per execution.
        limit: usize,
    },
    /// A statement does not begin with one of the supported commands.
    UnsupportedStatement {
        /// Zero-based statement index within the script.
        statement_index: usize,
        /// Zero-based byte position of the statement's first token.
        position: usize,
    },
    /// A `CREATE TABLE` statement failed.
    Create {
        /// Zero-based statement index within the script.
        statement_index: usize,
        /// The existing DDL implementation's error.
        source: CreateTableError,
    },
    /// An `INSERT INTO ... VALUES` statement failed.
    Insert {
        /// Zero-based statement index within the script.
        statement_index: usize,
        /// The existing DML implementation's error.
        source: InsertValuesError,
    },
    /// A scalar `SELECT` statement failed.
    ScalarSelect {
        /// Zero-based statement index within the script.
        statement_index: usize,
        /// The existing scalar SELECT implementation's error.
        source: ScalarSelectError,
    },
    /// A table projection `SELECT` statement failed.
    TableSelect {
        /// Zero-based statement index within the script.
        statement_index: usize,
        /// The existing table SELECT implementation's error.
        source: TableSelectError,
    },
}

impl fmt::Display for DatabaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lex(error) => error.fmt(formatter),
            Self::EmptyScript => formatter.write_str("SQL script contains no statements"),
            Self::EmptyStatement { position } => {
                write!(
                    formatter,
                    "SQL script contains an empty statement at byte {position}"
                )
            }
            Self::StatementLimitExceeded { limit } => {
                write!(formatter, "SQL script exceeds the {limit}-statement limit")
            }
            Self::UnsupportedStatement {
                statement_index,
                position,
            } => write!(
                formatter,
                "statement {} at byte {position} is not supported",
                statement_index + 1
            ),
            Self::Create {
                statement_index,
                source,
            } => write!(
                formatter,
                "statement {} failed: {source}",
                statement_index + 1
            ),
            Self::Insert {
                statement_index,
                source,
            } => write!(
                formatter,
                "statement {} failed: {source}",
                statement_index + 1
            ),
            Self::ScalarSelect {
                statement_index,
                source,
            } => write!(
                formatter,
                "statement {} failed: {source}",
                statement_index + 1
            ),
            Self::TableSelect {
                statement_index,
                source,
            } => write!(
                formatter,
                "statement {} failed: {source}",
                statement_index + 1
            ),
        }
    }
}

impl Error for DatabaseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Lex(error) => Some(error),
            Self::Create { source, .. } => Some(source),
            Self::Insert { source, .. } => Some(source),
            Self::ScalarSelect { source, .. } => Some(source),
            Self::TableSelect { source, .. } => Some(source),
            _ => None,
        }
    }
}
