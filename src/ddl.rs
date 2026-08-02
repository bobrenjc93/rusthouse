//! Parsing and execution for the first supported DDL statement.

use std::error::Error;
use std::fmt;

use crate::lexer::{Delimiter, LexError, LexerLimits, Token, TokenKind, lex};
use crate::{Catalog, CatalogError, ColumnSchema, DataType, Schema, SchemaError};

/// A parsed, schema-validated `CREATE TABLE` statement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateTableStatement {
    table_name: String,
    schema: Schema,
}

impl CreateTableStatement {
    /// Returns the table name exactly as parsed.
    #[must_use]
    pub fn table_name(&self) -> &str {
        &self.table_name
    }

    /// Returns the validated table schema.
    #[must_use]
    pub const fn schema(&self) -> &Schema {
        &self.schema
    }

    fn into_parts(self) -> (String, Schema) {
        (self.table_name, self.schema)
    }
}

/// An error returned while parsing or executing `CREATE TABLE`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CreateTableError {
    /// Tokenization failed.
    Lex(LexError),
    /// The token sequence does not match the supported statement shape.
    Syntax {
        /// Zero-based byte position at which parsing stopped.
        position: usize,
        /// Description of the expected syntax.
        expected: &'static str,
    },
    /// More than one semicolon-delimited statement was supplied.
    MultipleStatements {
        /// Zero-based byte position at which the extra statement begins.
        position: usize,
    },
    /// A column type is not one of the four storage types.
    UnknownType {
        /// The rejected type name.
        name: String,
        /// Zero-based byte position of the type name.
        position: usize,
    },
    /// The parsed column definitions do not form a valid schema.
    Schema(SchemaError),
    /// The catalog rejected the validated table definition.
    Catalog(CatalogError),
}

impl fmt::Display for CreateTableError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lex(error) => error.fmt(formatter),
            Self::Syntax { position, expected } => {
                write!(
                    formatter,
                    "SQL parse error at byte {position}: expected {expected}"
                )
            }
            Self::MultipleStatements { position } => write!(
                formatter,
                "SQL parse error at byte {position}: only one statement is allowed"
            ),
            Self::UnknownType { name, position } => write!(
                formatter,
                "SQL parse error at byte {position}: unknown column type `{name}`"
            ),
            Self::Schema(error) => write!(formatter, "invalid table schema: {error}"),
            Self::Catalog(error) => write!(formatter, "catalog error: {error}"),
        }
    }
}

impl Error for CreateTableError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Lex(error) => Some(error),
            Self::Schema(error) => Some(error),
            Self::Catalog(error) => Some(error),
            _ => None,
        }
    }
}

impl From<LexError> for CreateTableError {
    fn from(error: LexError) -> Self {
        Self::Lex(error)
    }
}

impl From<SchemaError> for CreateTableError {
    fn from(error: SchemaError) -> Self {
        Self::Schema(error)
    }
}

impl From<CatalogError> for CreateTableError {
    fn from(error: CatalogError) -> Self {
        Self::Catalog(error)
    }
}

/// Parses exactly one `CREATE TABLE name (column type [, column type ...])`.
///
/// The four type names and the `CREATE TABLE` keywords are ASCII
/// case-insensitive. A single trailing semicolon is optional, and the lexer
/// default limits bound the work performed.
pub fn parse_create_table(input: &str) -> Result<CreateTableStatement, CreateTableError> {
    let tokens = lex(input, LexerLimits::default())?;
    let mut cursor = Cursor::new(input, &tokens);

    cursor.expect_keyword("CREATE", "CREATE")?;
    cursor.expect_keyword("TABLE", "TABLE")?;
    let table_name = cursor.take_identifier("a table name")?;
    cursor.expect_delimiter(Delimiter::LeftParenthesis, "`(`")?;

    let mut columns = Vec::new();
    loop {
        let column_name = cursor.take_identifier("a column name")?;
        let data_type = cursor.take_data_type()?;
        columns.push(ColumnSchema::new(column_name, data_type));

        if cursor.take_delimiter(Delimiter::Comma) {
            continue;
        }
        cursor.expect_delimiter(Delimiter::RightParenthesis, "`,` or `)`")?;
        break;
    }

    if cursor.take_delimiter(Delimiter::Semicolon) {
        if !cursor.is_finished() {
            return Err(CreateTableError::MultipleStatements {
                position: cursor.position(),
            });
        }
    } else if !cursor.is_finished() {
        return Err(cursor.syntax("`;` or the end of the statement"));
    }

    let schema = Schema::new(columns)?;
    Ok(CreateTableStatement { table_name, schema })
}

/// Parses and creates one table in `catalog`.
///
/// Parsing and schema validation complete before the catalog is changed.
pub fn execute_create_table(catalog: &mut Catalog, input: &str) -> Result<(), CreateTableError> {
    let statement = parse_create_table(input)?;
    let (table_name, schema) = statement.into_parts();
    catalog.create_table(table_name, schema)?;
    Ok(())
}

struct Cursor<'a> {
    input: &'a str,
    tokens: &'a [Token],
    index: usize,
}

impl<'a> Cursor<'a> {
    const fn new(input: &'a str, tokens: &'a [Token]) -> Self {
        Self {
            input,
            tokens,
            index: 0,
        }
    }

    fn next(&mut self) -> Option<&'a Token> {
        let token = self.tokens.get(self.index)?;
        self.index += 1;
        Some(token)
    }

    fn position(&self) -> usize {
        self.tokens
            .get(self.index)
            .map_or(self.input.len(), |token| token.span.start)
    }

    fn syntax(&self, expected: &'static str) -> CreateTableError {
        CreateTableError::Syntax {
            position: self.position(),
            expected,
        }
    }

    fn is_finished(&self) -> bool {
        self.index == self.tokens.len()
    }

    fn expect_keyword(
        &mut self,
        keyword: &str,
        expected: &'static str,
    ) -> Result<(), CreateTableError> {
        let token = self.next().ok_or_else(|| self.syntax(expected))?;
        match &token.kind {
            TokenKind::Identifier(identifier) if identifier.eq_ignore_ascii_case(keyword) => Ok(()),
            _ => Err(CreateTableError::Syntax {
                position: token.span.start,
                expected,
            }),
        }
    }

    fn take_identifier(&mut self, expected: &'static str) -> Result<String, CreateTableError> {
        let token = self.next().ok_or_else(|| self.syntax(expected))?;
        match &token.kind {
            TokenKind::Identifier(identifier) | TokenKind::QuotedIdentifier(identifier) => {
                Ok(identifier.clone())
            }
            _ => Err(CreateTableError::Syntax {
                position: token.span.start,
                expected,
            }),
        }
    }

    fn take_data_type(&mut self) -> Result<DataType, CreateTableError> {
        let token = self
            .next()
            .ok_or_else(|| self.syntax("Int64, Float64, Bool, or String"))?;
        let TokenKind::Identifier(name) = &token.kind else {
            return Err(CreateTableError::Syntax {
                position: token.span.start,
                expected: "Int64, Float64, Bool, or String",
            });
        };

        if name.eq_ignore_ascii_case("Int64") {
            Ok(DataType::Int64)
        } else if name.eq_ignore_ascii_case("Float64") {
            Ok(DataType::Float64)
        } else if name.eq_ignore_ascii_case("Bool") {
            Ok(DataType::Bool)
        } else if name.eq_ignore_ascii_case("String") {
            Ok(DataType::String)
        } else {
            Err(CreateTableError::UnknownType {
                name: name.clone(),
                position: token.span.start,
            })
        }
    }

    fn expect_delimiter(
        &mut self,
        delimiter: Delimiter,
        expected: &'static str,
    ) -> Result<(), CreateTableError> {
        let token = self.next().ok_or_else(|| self.syntax(expected))?;
        if token.kind == TokenKind::Delimiter(delimiter) {
            Ok(())
        } else {
            Err(CreateTableError::Syntax {
                position: token.span.start,
                expected,
            })
        }
    }

    fn take_delimiter(&mut self, delimiter: Delimiter) -> bool {
        if self
            .tokens
            .get(self.index)
            .is_some_and(|token| token.kind == TokenKind::Delimiter(delimiter))
        {
            self.index += 1;
            true
        } else {
            false
        }
    }
}
