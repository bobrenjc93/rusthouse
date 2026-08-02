//! Parser for the SQL statements supported by RustHouse.

use std::error::Error;
use std::fmt;

use crate::sql::lexer::{LexError, LexErrorKind, LexerLimits, Span, Token, TokenKind, tokenize};
use crate::{ColumnDef, DataType};

/// The syntax tree for one `CREATE TABLE` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateTableStatement {
    /// The table name as written in the statement.
    pub name: String,
    /// The ordered column definitions in the table schema.
    pub columns: Vec<ColumnDef>,
}

/// Syntax elements that can be required while parsing a `CREATE TABLE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectedSyntax {
    /// The `CREATE` keyword.
    CreateKeyword,
    /// The `TABLE` keyword.
    TableKeyword,
    /// An unquoted table name.
    TableName,
    /// An opening parenthesis.
    LeftParenthesis,
    /// An unquoted column name.
    ColumnName,
    /// One of the supported column types.
    DataType,
    /// A comma or the closing parenthesis of the column list.
    CommaOrRightParenthesis,
}

impl fmt::Display for ExpectedSyntax {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::CreateKeyword => "CREATE",
            Self::TableKeyword => "TABLE",
            Self::TableName => "a table name",
            Self::LeftParenthesis => "(",
            Self::ColumnName => "a column name",
            Self::DataType => "a supported data type",
            Self::CommaOrRightParenthesis => ", or )",
        })
    }
}

/// The typed reason a `CREATE TABLE` statement could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseErrorKind {
    /// Tokenization failed before parsing could begin.
    Lexical(LexErrorKind),
    /// A required grammar element was absent or had the wrong token type.
    Expected(ExpectedSyntax),
    /// A column type name was syntactically valid but is not supported.
    UnsupportedType(String),
    /// Tokens remained after the statement and its optional terminator.
    TrailingInput,
}

/// A parse failure and its half-open byte range in the original SQL input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// The typed reason parsing failed.
    pub kind: ParseErrorKind,
    /// The token or end-of-input position at which parsing failed.
    pub span: Span,
}

impl ParseError {
    const fn new(kind: ParseErrorKind, span: Span) -> Self {
        Self { kind, span }
    }
}

impl From<LexError> for ParseError {
    fn from(error: LexError) -> Self {
        Self::new(ParseErrorKind::Lexical(error.kind), error.span)
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ParseErrorKind::Lexical(kind) => write!(formatter, "SQL lexical error: {kind:?}")?,
            ParseErrorKind::Expected(expected) => write!(formatter, "expected {expected}")?,
            ParseErrorKind::UnsupportedType(data_type) => {
                write!(formatter, "unsupported data type '{data_type}'")?
            }
            ParseErrorKind::TrailingInput => {
                formatter.write_str("unexpected input after CREATE TABLE statement")?
            }
        }

        write!(
            formatter,
            " at bytes {}..{}",
            self.span.start, self.span.end
        )
    }
}

impl Error for ParseError {}

/// Parses exactly one `CREATE TABLE name (column type, ...)` statement.
///
/// Keywords and the four type names (`Int64`, `Float64`, `Bool`, and
/// `String`) are ASCII case-insensitive. A single trailing semicolon is
/// optional. The supplied lexer limits are enforced before syntax parsing.
pub fn parse_create_table(
    input: &str,
    limits: LexerLimits,
) -> Result<CreateTableStatement, ParseError> {
    let tokens = tokenize(input, limits)?;
    Parser::new(&tokens, input.len()).parse_create_table()
}

struct Parser<'a> {
    tokens: &'a [Token],
    position: usize,
    input_len: usize,
}

impl<'a> Parser<'a> {
    const fn new(tokens: &'a [Token], input_len: usize) -> Self {
        Self {
            tokens,
            position: 0,
            input_len,
        }
    }

    fn parse_create_table(mut self) -> Result<CreateTableStatement, ParseError> {
        self.expect_keyword("CREATE", ExpectedSyntax::CreateKeyword)?;
        self.expect_keyword("TABLE", ExpectedSyntax::TableKeyword)?;
        let name = self.take_identifier(ExpectedSyntax::TableName)?;
        self.expect_token(TokenKind::LeftParen, ExpectedSyntax::LeftParenthesis)?;

        let mut columns = Vec::new();
        loop {
            let column_name = self.take_identifier(ExpectedSyntax::ColumnName)?;
            let data_type = self.take_data_type()?;
            columns.push(ColumnDef::new(column_name, data_type));

            match self.current() {
                Some(Token {
                    kind: TokenKind::Comma,
                    ..
                }) => self.position += 1,
                Some(Token {
                    kind: TokenKind::RightParen,
                    ..
                }) => {
                    self.position += 1;
                    break;
                }
                _ => return Err(self.expected(ExpectedSyntax::CommaOrRightParenthesis)),
            }
        }

        if self
            .current()
            .is_some_and(|token| token.kind == TokenKind::StatementTerminator)
        {
            self.position += 1;
        }

        if let Some(token) = self.current() {
            return Err(ParseError::new(ParseErrorKind::TrailingInput, token.span));
        }

        Ok(CreateTableStatement { name, columns })
    }

    fn expect_keyword(
        &mut self,
        keyword: &str,
        expected: ExpectedSyntax,
    ) -> Result<(), ParseError> {
        match self.current() {
            Some(Token {
                kind: TokenKind::Identifier(identifier),
                ..
            }) if identifier.eq_ignore_ascii_case(keyword) => {
                self.position += 1;
                Ok(())
            }
            _ => Err(self.expected(expected)),
        }
    }

    fn take_identifier(&mut self, expected: ExpectedSyntax) -> Result<String, ParseError> {
        match self.current() {
            Some(Token {
                kind: TokenKind::Identifier(identifier),
                ..
            }) => {
                let identifier = identifier.clone();
                self.position += 1;
                Ok(identifier)
            }
            _ => Err(self.expected(expected)),
        }
    }

    fn take_data_type(&mut self) -> Result<DataType, ParseError> {
        let Some(token) = self.current() else {
            return Err(self.expected(ExpectedSyntax::DataType));
        };
        let TokenKind::Identifier(identifier) = &token.kind else {
            return Err(self.expected(ExpectedSyntax::DataType));
        };

        let data_type = if identifier.eq_ignore_ascii_case("Int64") {
            DataType::Int64
        } else if identifier.eq_ignore_ascii_case("Float64") {
            DataType::Float64
        } else if identifier.eq_ignore_ascii_case("Bool") {
            DataType::Bool
        } else if identifier.eq_ignore_ascii_case("String") {
            DataType::String
        } else {
            return Err(ParseError::new(
                ParseErrorKind::UnsupportedType(identifier.clone()),
                token.span,
            ));
        };

        self.position += 1;
        Ok(data_type)
    }

    fn expect_token(
        &mut self,
        expected_kind: TokenKind,
        expected: ExpectedSyntax,
    ) -> Result<(), ParseError> {
        if self
            .current()
            .is_some_and(|token| token.kind == expected_kind)
        {
            self.position += 1;
            Ok(())
        } else {
            Err(self.expected(expected))
        }
    }

    fn expected(&self, expected: ExpectedSyntax) -> ParseError {
        ParseError::new(ParseErrorKind::Expected(expected), self.current_span())
    }

    fn current(&self) -> Option<&'a Token> {
        self.tokens.get(self.position)
    }

    fn current_span(&self) -> Span {
        self.current()
            .map_or(Span::new(self.input_len, self.input_len), |token| {
                token.span
            })
    }
}
