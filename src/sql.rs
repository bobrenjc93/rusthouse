use std::fmt;

/// Maximum number of statements accepted in one SQL batch.
pub const MAX_BATCH_STATEMENTS: usize = 256;
/// Maximum number of lexical tokens accepted in one SQL batch.
pub const MAX_BATCH_TOKENS: usize = 16_384;
/// Maximum number of literal projections accepted in one `SELECT`.
pub const MAX_SELECT_PROJECTIONS: usize = 1024;

/// A scalar value produced by a literal projection.
#[derive(Clone, Debug, PartialEq)]
pub enum ScalarValue {
    Integer(i64),
    Float(f64),
    Boolean(bool),
    String(String),
}

impl ScalarValue {
    pub fn to_output_string(&self) -> String {
        match self {
            Self::Integer(value) => value.to_string(),
            Self::Float(value) => value.to_string(),
            Self::Boolean(value) => value.to_string(),
            Self::String(value) => value.clone(),
        }
    }
}

/// A named value in a query result.
#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    pub name: String,
    pub value: ScalarValue,
}

/// The single-row result of a literal `SELECT` statement.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryResult {
    pub columns: Vec<Column>,
}

/// The category of a SQL error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SqlErrorKind {
    Syntax {
        message: String,
    },
    UnsupportedClause {
        clause: String,
    },
    UnsupportedStatement {
        statement: String,
    },
    LimitExceeded {
        resource: &'static str,
        maximum: usize,
    },
}

/// A SQL error with a one-based byte position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqlError {
    pub byte_offset: usize,
    pub kind: SqlErrorKind,
}

impl SqlError {
    fn syntax(offset: usize, message: impl Into<String>) -> Self {
        Self {
            byte_offset: offset + 1,
            kind: SqlErrorKind::Syntax {
                message: message.into(),
            },
        }
    }

    fn unsupported_clause(offset: usize, clause: &str) -> Self {
        Self {
            byte_offset: offset + 1,
            kind: SqlErrorKind::UnsupportedClause {
                clause: clause.to_ascii_uppercase(),
            },
        }
    }

    fn unsupported_statement(offset: usize, statement: &str) -> Self {
        Self {
            byte_offset: offset + 1,
            kind: SqlErrorKind::UnsupportedStatement {
                statement: statement.to_ascii_uppercase(),
            },
        }
    }

    fn limit_exceeded(offset: usize, resource: &'static str, maximum: usize) -> Self {
        Self {
            byte_offset: offset + 1,
            kind: SqlErrorKind::LimitExceeded { resource, maximum },
        }
    }

    fn from_kind(offset: usize, kind: SqlErrorKind) -> Self {
        Self {
            byte_offset: offset + 1,
            kind,
        }
    }

    fn into_token(self) -> Token {
        Token {
            kind: TokenKind::Failure(self.kind),
            offset: self.byte_offset - 1,
        }
    }
}

impl fmt::Display for SqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            SqlErrorKind::Syntax { message } => {
                write!(
                    f,
                    "SQL syntax error at byte {}: {message}",
                    self.byte_offset
                )
            }
            SqlErrorKind::UnsupportedClause { clause } => write!(
                f,
                "unsupported SQL clause `{clause}` at byte {}; only literal SELECT projections are supported",
                self.byte_offset
            ),
            SqlErrorKind::UnsupportedStatement { statement } => write!(
                f,
                "unsupported SQL statement `{statement}` at byte {}; only SELECT is supported",
                self.byte_offset
            ),
            SqlErrorKind::LimitExceeded { resource, maximum } => write!(
                f,
                "SQL {resource} limit exceeded at byte {}; maximum is {maximum}",
                self.byte_offset
            ),
        }
    }
}

impl std::error::Error for SqlError {}

/// Parses and executes every statement in a SQL batch.
pub fn execute_batch(input: &str) -> Result<Vec<QueryResult>, SqlError> {
    let tokens = Lexer::new(input).tokenize();
    Parser::new(tokens).parse_batch()
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TokenKind {
    Word(String),
    Number(String),
    String(String),
    QuotedIdentifier(String),
    Comma,
    Semicolon,
    Plus,
    Minus,
    Star,
    Failure(SqlErrorKind),
    End,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Token {
    kind: TokenKind,
    offset: usize,
}

struct Lexer<'a> {
    input: &'a str,
    position: usize,
}

impl<'a> Lexer<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, position: 0 }
    }

    fn tokenize(mut self) -> Vec<Token> {
        let mut tokens = Vec::new();
        while let Some(character) = self.peek() {
            if character.is_whitespace() {
                self.bump();
                continue;
            }

            if tokens.len() >= MAX_BATCH_TOKENS {
                tokens.push(Token {
                    kind: TokenKind::Failure(SqlErrorKind::LimitExceeded {
                        resource: "token",
                        maximum: MAX_BATCH_TOKENS,
                    }),
                    offset: self.position,
                });
                break;
            }

            let offset = self.position;
            let kind = match character {
                ',' => {
                    self.bump();
                    TokenKind::Comma
                }
                ';' => {
                    self.bump();
                    TokenKind::Semicolon
                }
                '+' => {
                    self.bump();
                    TokenKind::Plus
                }
                '-' if self.peek_next() == Some('-') => {
                    self.skip_line_comment();
                    continue;
                }
                '-' => {
                    self.bump();
                    TokenKind::Minus
                }
                '*' => {
                    self.bump();
                    TokenKind::Star
                }
                '/' if self.peek_next() == Some('*') => match self.skip_block_comment(offset) {
                    Ok(()) => continue,
                    Err(error) => {
                        tokens.push(error.into_token());
                        break;
                    }
                },
                '\'' => match self.quoted('\'', "string literal", offset) {
                    Ok(value) => TokenKind::String(value),
                    Err(error) => {
                        tokens.push(error.into_token());
                        break;
                    }
                },
                '"' => match self.quoted('"', "quoted identifier", offset) {
                    Ok(value) => TokenKind::QuotedIdentifier(value),
                    Err(error) => {
                        tokens.push(error.into_token());
                        break;
                    }
                },
                '.' if self.peek_next().is_some_and(|next| next.is_ascii_digit()) => {
                    match self.number(offset) {
                        Ok(value) => TokenKind::Number(value),
                        Err(error) => {
                            tokens.push(error.into_token());
                            break;
                        }
                    }
                }
                value if value.is_ascii_digit() => match self.number(offset) {
                    Ok(value) => TokenKind::Number(value),
                    Err(error) => {
                        tokens.push(error.into_token());
                        break;
                    }
                },
                value if value.is_ascii_alphabetic() || value == '_' => {
                    TokenKind::Word(self.word())
                }
                value => {
                    self.bump();
                    TokenKind::Failure(SqlErrorKind::Syntax {
                        message: format!("unexpected character `{}`", value.escape_default()),
                    })
                }
            };
            let failed = matches!(kind, TokenKind::Failure(_));
            tokens.push(Token { kind, offset });
            if failed {
                break;
            }
        }

        tokens.push(Token {
            kind: TokenKind::End,
            offset: self.input.len(),
        });
        tokens
    }

    fn peek(&self) -> Option<char> {
        self.input[self.position..].chars().next()
    }

    fn peek_next(&self) -> Option<char> {
        let mut characters = self.input[self.position..].chars();
        characters.next()?;
        characters.next()
    }

    fn bump(&mut self) -> Option<char> {
        let character = self.peek()?;
        self.position += character.len_utf8();
        Some(character)
    }

    fn word(&mut self) -> String {
        let start = self.position;
        while self
            .peek()
            .is_some_and(|value| value.is_ascii_alphanumeric() || value == '_')
        {
            self.bump();
        }
        self.input[start..self.position].to_owned()
    }

    fn number(&mut self, offset: usize) -> Result<String, SqlError> {
        if self.peek() == Some('.') {
            self.bump();
        }
        while self.peek().is_some_and(|value| value.is_ascii_digit()) {
            self.bump();
        }
        if self.peek() == Some('.') {
            self.bump();
            while self.peek().is_some_and(|value| value.is_ascii_digit()) {
                self.bump();
            }
        }
        if self.peek().is_some_and(|value| matches!(value, 'e' | 'E')) {
            self.bump();
            if self.peek().is_some_and(|value| matches!(value, '+' | '-')) {
                self.bump();
            }
            if !self.peek().is_some_and(|value| value.is_ascii_digit()) {
                return Err(SqlError::syntax(offset, "invalid numeric literal"));
            }
            while self.peek().is_some_and(|value| value.is_ascii_digit()) {
                self.bump();
            }
        }
        if self
            .peek()
            .is_some_and(|value| value.is_ascii_alphabetic() || value == '_')
        {
            while self
                .peek()
                .is_some_and(|value| value.is_ascii_alphanumeric() || value == '_')
            {
                self.bump();
            }
            return Err(SqlError::syntax(
                offset,
                format!(
                    "invalid numeric literal `{}`",
                    &self.input[offset..self.position]
                ),
            ));
        }
        Ok(self.input[offset..self.position].to_owned())
    }

    fn quoted(
        &mut self,
        delimiter: char,
        description: &str,
        offset: usize,
    ) -> Result<String, SqlError> {
        debug_assert_eq!(self.bump(), Some(delimiter));
        let mut value = String::new();
        loop {
            match self.bump() {
                Some(character) if character == delimiter => {
                    if self.peek() == Some(delimiter) {
                        self.bump();
                        value.push(delimiter);
                    } else {
                        return Ok(value);
                    }
                }
                Some(character) => value.push(character),
                None => {
                    return Err(SqlError::syntax(
                        offset,
                        format!("unterminated {description}"),
                    ));
                }
            }
        }
    }

    fn skip_line_comment(&mut self) {
        self.bump();
        self.bump();
        while self.peek().is_some_and(|value| value != '\n') {
            self.bump();
        }
    }

    fn skip_block_comment(&mut self, offset: usize) -> Result<(), SqlError> {
        self.bump();
        self.bump();
        loop {
            match (self.peek(), self.peek_next()) {
                (Some('*'), Some('/')) => {
                    self.bump();
                    self.bump();
                    return Ok(());
                }
                (Some(_), _) => {
                    self.bump();
                }
                (None, _) => {
                    return Err(SqlError::syntax(offset, "unterminated block comment"));
                }
            }
        }
    }
}

struct Parser {
    tokens: Vec<Token>,
    position: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self {
            tokens,
            position: 0,
        }
    }

    fn parse_batch(mut self) -> Result<Vec<QueryResult>, SqlError> {
        let mut results = Vec::new();
        while !matches!(self.current().kind, TokenKind::End) {
            if matches!(self.current().kind, TokenKind::Semicolon) {
                self.advance();
                continue;
            }
            if results.len() >= MAX_BATCH_STATEMENTS {
                return Err(SqlError::limit_exceeded(
                    self.current().offset,
                    "statement",
                    MAX_BATCH_STATEMENTS,
                ));
            }
            results.push(self.parse_statement()?);
            match &self.current().kind {
                TokenKind::Semicolon => self.advance(),
                TokenKind::End => {}
                TokenKind::Word(word) if word.eq_ignore_ascii_case("SELECT") => {
                    return Err(SqlError::syntax(
                        self.current().offset,
                        "expected `;` between SELECT statements",
                    ));
                }
                TokenKind::Failure(kind) => {
                    return Err(SqlError::from_kind(self.current().offset, kind.clone()));
                }
                _ => {
                    return Err(SqlError::syntax(
                        self.current().offset,
                        "expected `;` after SELECT statement",
                    ));
                }
            }
        }

        if results.is_empty() {
            return Err(SqlError::syntax(0, "expected a SELECT statement"));
        }
        Ok(results)
    }

    fn parse_statement(&mut self) -> Result<QueryResult, SqlError> {
        match &self.current().kind {
            TokenKind::Word(word) if word.eq_ignore_ascii_case("SELECT") => self.advance(),
            TokenKind::Word(word) => {
                return Err(SqlError::unsupported_statement(self.current().offset, word));
            }
            TokenKind::Failure(kind) => {
                return Err(SqlError::from_kind(self.current().offset, kind.clone()));
            }
            _ => {
                return Err(SqlError::syntax(
                    self.current().offset,
                    "expected a SELECT statement",
                ));
            }
        }

        let mut columns = Vec::new();
        loop {
            if columns.len() >= MAX_SELECT_PROJECTIONS {
                return Err(SqlError::limit_exceeded(
                    self.current().offset,
                    "projection",
                    MAX_SELECT_PROJECTIONS,
                ));
            }
            columns.push(self.parse_select_item()?);
            match &self.current().kind {
                TokenKind::Comma => {
                    self.advance();
                }
                TokenKind::Semicolon | TokenKind::End => break,
                TokenKind::Word(word) if is_unsupported_clause(word) => {
                    return Err(SqlError::unsupported_clause(self.current().offset, word));
                }
                TokenKind::Failure(kind) => {
                    return Err(SqlError::from_kind(self.current().offset, kind.clone()));
                }
                _ => {
                    return Err(SqlError::syntax(
                        self.current().offset,
                        "expected `,` or the end of the SELECT statement",
                    ));
                }
            }
        }

        Ok(QueryResult { columns })
    }

    fn parse_select_item(&mut self) -> Result<Column, SqlError> {
        let (value, default_name) = self.parse_literal()?;
        let name = if self.current_word_is("AS") {
            self.advance();
            self.parse_alias("expected an alias after AS")?
        } else {
            match &self.current().kind {
                TokenKind::QuotedIdentifier(_) => self.parse_alias("expected an alias")?,
                TokenKind::Word(word)
                    if !is_unsupported_clause(word) && !word.eq_ignore_ascii_case("SELECT") =>
                {
                    self.parse_alias("expected an alias")?
                }
                _ => default_name,
            }
        };

        Ok(Column { name, value })
    }

    fn parse_literal(&mut self) -> Result<(ScalarValue, String), SqlError> {
        let sign = match self.current().kind {
            TokenKind::Plus => {
                self.advance();
                "+"
            }
            TokenKind::Minus => {
                self.advance();
                "-"
            }
            _ => "",
        };

        let token = self.current().clone();
        match token.kind {
            TokenKind::Number(number) => {
                self.advance();
                let literal = format!("{sign}{number}");
                if number.contains(['.', 'e', 'E']) {
                    let value = literal.parse::<f64>().map_err(|_| {
                        SqlError::syntax(token.offset, format!("invalid float literal `{literal}`"))
                    })?;
                    if !value.is_finite() {
                        return Err(SqlError::syntax(
                            token.offset,
                            format!("float literal `{literal}` is outside the finite range"),
                        ));
                    }
                    Ok((ScalarValue::Float(value), literal))
                } else {
                    let value = literal.parse::<i64>().map_err(|_| {
                        SqlError::syntax(
                            token.offset,
                            format!("integer literal `{literal}` is outside the Int64 range"),
                        )
                    })?;
                    Ok((ScalarValue::Integer(value), literal))
                }
            }
            TokenKind::String(value) if sign.is_empty() => {
                self.advance();
                Ok((ScalarValue::String(value.clone()), format!("'{value}'")))
            }
            TokenKind::Word(word)
                if sign.is_empty()
                    && (word.eq_ignore_ascii_case("TRUE")
                        || word.eq_ignore_ascii_case("FALSE")) =>
            {
                self.advance();
                let value = word.eq_ignore_ascii_case("TRUE");
                Ok((ScalarValue::Boolean(value), value.to_string()))
            }
            TokenKind::Failure(kind) => Err(SqlError::from_kind(token.offset, kind)),
            TokenKind::End | TokenKind::Semicolon | TokenKind::Comma => Err(SqlError::syntax(
                token.offset,
                "expected an integer, float, boolean, or string literal",
            )),
            other => Err(SqlError::syntax(
                token.offset,
                format!(
                    "expected a scalar literal, found {}",
                    describe_token(&other)
                ),
            )),
        }
    }

    fn parse_alias(&mut self, message: &str) -> Result<String, SqlError> {
        let token = self.current().clone();
        let alias = match token.kind {
            TokenKind::Word(alias) | TokenKind::QuotedIdentifier(alias) if !alias.is_empty() => {
                alias
            }
            TokenKind::Failure(kind) => return Err(SqlError::from_kind(token.offset, kind)),
            _ => return Err(SqlError::syntax(token.offset, message)),
        };
        self.advance();
        Ok(alias)
    }

    fn current_word_is(&self, expected: &str) -> bool {
        matches!(&self.current().kind, TokenKind::Word(word) if word.eq_ignore_ascii_case(expected))
    }

    fn current(&self) -> &Token {
        &self.tokens[self.position]
    }

    fn advance(&mut self) {
        self.position += 1;
    }
}

fn is_unsupported_clause(word: &str) -> bool {
    [
        "FROM",
        "PREWHERE",
        "WHERE",
        "GROUP",
        "WITH",
        "TOTALS",
        "HAVING",
        "WINDOW",
        "QUALIFY",
        "ORDER",
        "LIMIT",
        "OFFSET",
        "FETCH",
        "FOR",
        "JOIN",
        "UNION",
        "EXCEPT",
        "INTERSECT",
        "INTO",
        "FORMAT",
        "SETTINGS",
    ]
    .iter()
    .any(|clause| word.eq_ignore_ascii_case(clause))
}

fn describe_token(token: &TokenKind) -> String {
    match token {
        TokenKind::Word(value) | TokenKind::Number(value) => format!("`{value}`"),
        TokenKind::String(_) => "a string literal".to_owned(),
        TokenKind::QuotedIdentifier(value) => {
            format!("identifier `{}`", escape_for_diagnostic(value))
        }
        TokenKind::Comma => "`,`".to_owned(),
        TokenKind::Semicolon => "`;`".to_owned(),
        TokenKind::Plus => "`+`".to_owned(),
        TokenKind::Minus => "`-`".to_owned(),
        TokenKind::Star => "`*`".to_owned(),
        TokenKind::Failure(_) => "an invalid token".to_owned(),
        TokenKind::End => "the end of input".to_owned(),
    }
}

fn escape_for_diagnostic(value: &str) -> String {
    value.chars().flat_map(char::escape_default).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_typed_literals_and_aliases() {
        let results = execute_batch(
            "SELECT -42 AS integer_value, 1.25e2 float_value, FALSE AS enabled, \
             'it''s SQL' AS \"display text\";",
        )
        .unwrap();

        assert_eq!(
            results[0].columns,
            vec![
                Column {
                    name: "integer_value".to_owned(),
                    value: ScalarValue::Integer(-42),
                },
                Column {
                    name: "float_value".to_owned(),
                    value: ScalarValue::Float(125.0),
                },
                Column {
                    name: "enabled".to_owned(),
                    value: ScalarValue::Boolean(false),
                },
                Column {
                    name: "display text".to_owned(),
                    value: ScalarValue::String("it's SQL".to_owned()),
                },
            ]
        );
    }

    #[test]
    fn parses_multiple_statements_and_comments() {
        let results =
            execute_batch("-- first\nSELECT 1 AS one; /* next */ SELECT true AS two;").unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn reports_unsupported_clauses() {
        let error = execute_batch("SELECT 1 FROM numbers WHERE value = 1;").unwrap_err();
        assert!(matches!(
            error.kind,
            SqlErrorKind::UnsupportedClause { ref clause } if clause == "FROM"
        ));
    }

    #[test]
    fn reports_unsupported_statements_before_later_lexical_failures() {
        let error = execute_batch("CREATE TABLE values (value Int64);").unwrap_err();
        assert!(matches!(
            error.kind,
            SqlErrorKind::UnsupportedStatement { ref statement } if statement == "CREATE"
        ));
    }

    #[test]
    fn rejects_non_finite_and_out_of_range_numbers() {
        assert!(execute_batch("SELECT 1e999 AS huge;").is_err());
        assert!(execute_batch("SELECT 9223372036854775808 AS huge;").is_err());
    }

    #[test]
    fn rejects_identifier_characters_attached_to_numbers() {
        for input in ["SELECT 123abc;", "SELECT 12.5value;", "SELECT 1e2seconds;"] {
            let error = execute_batch(input).unwrap_err();
            assert!(
                matches!(error.kind, SqlErrorKind::Syntax { ref message } if message.starts_with("invalid numeric literal")),
                "unexpected error for {input}: {error}"
            );
        }
    }

    #[test]
    fn bounds_projection_statement_and_token_counts() {
        let projections = format!(
            "SELECT {};",
            std::iter::repeat_n("1", MAX_SELECT_PROJECTIONS + 1)
                .collect::<Vec<_>>()
                .join(",")
        );
        let error = execute_batch(&projections).unwrap_err();
        assert!(matches!(
            error.kind,
            SqlErrorKind::LimitExceeded {
                resource: "projection",
                maximum: MAX_SELECT_PROJECTIONS
            }
        ));

        let statements = "SELECT 1;".repeat(MAX_BATCH_STATEMENTS + 1);
        let error = execute_batch(&statements).unwrap_err();
        assert!(matches!(
            error.kind,
            SqlErrorKind::LimitExceeded {
                resource: "statement",
                maximum: MAX_BATCH_STATEMENTS
            }
        ));

        let tokens = ";".repeat(MAX_BATCH_TOKENS + 1);
        let error = execute_batch(&tokens).unwrap_err();
        assert!(matches!(
            error.kind,
            SqlErrorKind::LimitExceeded {
                resource: "token",
                maximum: MAX_BATCH_TOKENS
            }
        ));
    }
}
