use crate::error::{Error, Result};
use crate::storage::{DataType, Value};

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Statement {
    Create(CreateTable),
    Insert(Insert),
    Select(Select),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CreateTable {
    pub name: String,
    pub if_not_exists: bool,
    pub columns: Vec<ColumnDefinition>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ColumnDefinition {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Insert {
    pub table: String,
    pub columns: Option<Vec<String>>,
    pub rows: Vec<Vec<Expr>>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Select {
    pub distinct: bool,
    pub items: Vec<SelectItem>,
    pub table: String,
    pub filter: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    pub order_by: Vec<OrderBy>,
    pub limit: Option<usize>,
    pub offset: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SelectItem {
    pub expression: Expr,
    pub alias: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OrderBy {
    pub expression: Expr,
    pub ascending: bool,
    pub nulls_first: Option<bool>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Expr {
    Column(String),
    Literal(Value),
    Wildcard,
    Function {
        name: String,
        arguments: Vec<Expr>,
        distinct: bool,
    },
    Binary {
        left: Box<Expr>,
        operator: BinaryOperator,
        right: Box<Expr>,
    },
    Unary {
        operator: UnaryOperator,
        expression: Box<Expr>,
    },
    IsNull {
        expression: Box<Expr>,
        negated: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BinaryOperator {
    Or,
    And,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    Add,
    Subtract,
    Multiply,
    Divide,
    Modulo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnaryOperator {
    Not,
    Negate,
    Positive,
}

#[derive(Clone, Debug, PartialEq)]
enum TokenKind {
    Word(String),
    Number(String),
    String(String),
    Comma,
    LeftParen,
    RightParen,
    Semicolon,
    Star,
    Plus,
    Minus,
    Slash,
    Percent,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    End,
}

#[derive(Clone, Debug)]
struct Token {
    kind: TokenKind,
    offset: usize,
}

pub(crate) fn parse(sql: &str) -> Result<Vec<Statement>> {
    let tokens = Lexer::new(sql).tokenize()?;
    Parser::new(tokens).parse_script()
}

struct Lexer<'a> {
    source: &'a str,
    offset: usize,
}

impl<'a> Lexer<'a> {
    fn new(source: &'a str) -> Self {
        Self { source, offset: 0 }
    }

    fn tokenize(mut self) -> Result<Vec<Token>> {
        let mut tokens = Vec::new();
        loop {
            self.skip_trivia()?;
            let offset = self.offset;
            let Some(character) = self.peek() else {
                tokens.push(Token {
                    kind: TokenKind::End,
                    offset,
                });
                return Ok(tokens);
            };
            let kind = match character {
                ',' => self.single(TokenKind::Comma),
                '(' => self.single(TokenKind::LeftParen),
                ')' => self.single(TokenKind::RightParen),
                ';' => self.single(TokenKind::Semicolon),
                '*' => self.single(TokenKind::Star),
                '+' => self.single(TokenKind::Plus),
                '-' => self.single(TokenKind::Minus),
                '/' => self.single(TokenKind::Slash),
                '%' => self.single(TokenKind::Percent),
                '=' => self.single(TokenKind::Equal),
                '<' => {
                    self.bump();
                    match self.peek() {
                        Some('=') => {
                            self.bump();
                            TokenKind::LessEqual
                        }
                        Some('>') => {
                            self.bump();
                            TokenKind::NotEqual
                        }
                        _ => TokenKind::Less,
                    }
                }
                '>' => {
                    self.bump();
                    if self.peek() == Some('=') {
                        self.bump();
                        TokenKind::GreaterEqual
                    } else {
                        TokenKind::Greater
                    }
                }
                '!' => {
                    self.bump();
                    if self.peek() != Some('=') {
                        return Err(self.error(offset, "expected '=' after '!'"));
                    }
                    self.bump();
                    TokenKind::NotEqual
                }
                '\'' => TokenKind::String(self.string_literal(offset)?),
                '"' | '`' => TokenKind::Word(self.quoted_identifier(character, offset)?),
                c if c.is_ascii_digit() || (c == '.' && self.peek_second_is_digit()) => {
                    TokenKind::Number(self.number(offset)?)
                }
                c if is_identifier_start(c) => TokenKind::Word(self.word()),
                _ => {
                    return Err(self.error(offset, format!("unexpected character {character:?}")));
                }
            };
            tokens.push(Token { kind, offset });
        }
    }

    fn single(&mut self, kind: TokenKind) -> TokenKind {
        self.bump();
        kind
    }

    fn skip_trivia(&mut self) -> Result<()> {
        loop {
            while self.peek().is_some_and(char::is_whitespace) {
                self.bump();
            }
            if self.remaining().starts_with("--") {
                while self.peek().is_some_and(|c| c != '\n') {
                    self.bump();
                }
            } else if self.remaining().starts_with("/*") {
                let start = self.offset;
                self.offset += 2;
                let Some(end) = self.remaining().find("*/") else {
                    return Err(self.error(start, "unterminated block comment"));
                };
                self.offset += end + 2;
            } else {
                return Ok(());
            }
        }
    }

    fn string_literal(&mut self, start: usize) -> Result<String> {
        self.bump();
        let mut value = String::new();
        loop {
            let Some(character) = self.bump() else {
                return Err(self.error(start, "unterminated string literal"));
            };
            match character {
                '\'' if self.peek() == Some('\'') => {
                    self.bump();
                    value.push('\'');
                }
                '\'' => return Ok(value),
                '\\' => {
                    let Some(escaped) = self.bump() else {
                        return Err(self.error(start, "unterminated string escape"));
                    };
                    value.push(match escaped {
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        '0' => '\0',
                        other => other,
                    });
                }
                other => value.push(other),
            }
        }
    }

    fn quoted_identifier(&mut self, quote: char, start: usize) -> Result<String> {
        self.bump();
        let mut value = String::new();
        loop {
            let Some(character) = self.bump() else {
                return Err(self.error(start, "unterminated quoted identifier"));
            };
            if character == quote {
                if self.peek() == Some(quote) {
                    self.bump();
                    value.push(quote);
                } else {
                    return Ok(value);
                }
            } else {
                value.push(character);
            }
        }
    }

    fn number(&mut self, start: usize) -> Result<String> {
        let beginning = self.offset;
        let mut digits = 0;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            digits += 1;
            self.bump();
        }
        if self.peek() == Some('.') {
            self.bump();
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                digits += 1;
                self.bump();
            }
        }
        if digits == 0 {
            return Err(self.error(start, "invalid numeric literal"));
        }
        if self.peek().is_some_and(|c| c == 'e' || c == 'E') {
            self.bump();
            if self.peek().is_some_and(|c| c == '+' || c == '-') {
                self.bump();
            }
            let exponent = self.offset;
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.bump();
            }
            if exponent == self.offset {
                return Err(self.error(start, "invalid numeric exponent"));
            }
        }
        Ok(self.source[beginning..self.offset].to_owned())
    }

    fn word(&mut self) -> String {
        let beginning = self.offset;
        self.bump();
        while self.peek().is_some_and(is_identifier_continue) {
            self.bump();
        }
        self.source[beginning..self.offset].to_ascii_lowercase()
    }

    fn remaining(&self) -> &'a str {
        &self.source[self.offset..]
    }

    fn peek(&self) -> Option<char> {
        self.remaining().chars().next()
    }

    fn peek_second_is_digit(&self) -> bool {
        self.remaining()
            .chars()
            .nth(1)
            .is_some_and(|c| c.is_ascii_digit())
    }

    fn bump(&mut self) -> Option<char> {
        let character = self.peek()?;
        self.offset += character.len_utf8();
        Some(character)
    }

    fn error(&self, offset: usize, message: impl std::fmt::Display) -> Error {
        Error::new(format!("SQL error at byte {offset}: {message}"))
    }
}

fn is_identifier_start(character: char) -> bool {
    character == '_' || character.is_alphabetic()
}

fn is_identifier_continue(character: char) -> bool {
    character == '_' || character.is_alphanumeric()
}

struct Parser {
    tokens: Vec<Token>,
    index: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, index: 0 }
    }

    fn parse_script(mut self) -> Result<Vec<Statement>> {
        let mut statements = Vec::new();
        while !self.is_end() {
            if self.consume(&TokenKind::Semicolon) {
                continue;
            }
            statements.push(self.parse_statement()?);
            if !self.consume(&TokenKind::Semicolon) && !self.is_end() {
                return Err(self.expected("';' between statements"));
            }
        }
        Ok(statements)
    }

    fn parse_statement(&mut self) -> Result<Statement> {
        if self.consume_keyword("create") {
            self.expect_keyword("table")?;
            self.parse_create().map(Statement::Create)
        } else if self.consume_keyword("insert") {
            self.parse_insert().map(Statement::Insert)
        } else if self.consume_keyword("select") {
            self.parse_select().map(Statement::Select)
        } else {
            Err(self.expected("CREATE, INSERT, or SELECT"))
        }
    }

    fn parse_create(&mut self) -> Result<CreateTable> {
        let if_not_exists = if self.consume_keyword("if") {
            self.expect_keyword("not")?;
            self.expect_keyword("exists")?;
            true
        } else {
            false
        };
        let name = self.identifier()?;
        self.expect(&TokenKind::LeftParen, "'('")?;
        let mut columns = Vec::new();
        loop {
            let name = self.identifier()?;
            let (data_type, mut nullable) = self.data_type()?;
            if self.consume_keyword("null") {
                nullable = true;
            } else if self.consume_keyword("not") {
                self.expect_keyword("null")?;
                nullable = false;
            }
            columns.push(ColumnDefinition {
                name,
                data_type,
                nullable,
            });
            if !self.consume(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RightParen, "')'")?;
        if self.consume_keyword("engine") {
            self.consume(&TokenKind::Equal);
            let engine = self.identifier()?;
            if !engine.eq_ignore_ascii_case("memory") {
                return Err(Error::new(format!(
                    "unsupported table engine '{engine}'; only Memory is available"
                )));
            }
        }
        self.expect_statement_end()?;
        Ok(CreateTable {
            name,
            if_not_exists,
            columns,
        })
    }

    fn data_type(&mut self) -> Result<(DataType, bool)> {
        if self.consume_keyword("nullable") {
            self.expect(&TokenKind::LeftParen, "'('")?;
            let (kind, nested_nullable) = self.data_type()?;
            self.expect(&TokenKind::RightParen, "')'")?;
            if nested_nullable {
                return Err(Error::new("nested Nullable types are not supported"));
            }
            return Ok((kind, true));
        }
        let name = self.identifier()?;
        let kind = match name.to_ascii_lowercase().as_str() {
            "int64" => DataType::Int64,
            "float64" => DataType::Float64,
            "bool" | "boolean" => DataType::Bool,
            "string" => DataType::String,
            _ => return Err(Error::new(format!("unsupported data type '{name}'"))),
        };
        Ok((kind, false))
    }

    fn parse_insert(&mut self) -> Result<Insert> {
        self.expect_keyword("into")?;
        let table = self.identifier()?;
        let columns = if self.consume(&TokenKind::LeftParen) {
            let mut names = Vec::new();
            loop {
                names.push(self.identifier()?);
                if !self.consume(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RightParen, "')'")?;
            Some(names)
        } else {
            None
        };
        self.expect_keyword("values")?;
        let mut rows = Vec::new();
        loop {
            self.expect(&TokenKind::LeftParen, "'('")?;
            let mut row = Vec::new();
            if !self.check(&TokenKind::RightParen) {
                loop {
                    row.push(self.expression()?);
                    if !self.consume(&TokenKind::Comma) {
                        break;
                    }
                }
            }
            self.expect(&TokenKind::RightParen, "')'")?;
            rows.push(row);
            if !self.consume(&TokenKind::Comma) {
                break;
            }
        }
        self.expect_statement_end()?;
        Ok(Insert {
            table,
            columns,
            rows,
        })
    }

    fn parse_select(&mut self) -> Result<Select> {
        let distinct = self.consume_keyword("distinct");
        let mut items = Vec::new();
        loop {
            let expression = self.expression()?;
            let alias = if self.consume_keyword("as") || self.next_is_implicit_alias() {
                Some(self.identifier()?)
            } else {
                None
            };
            items.push(SelectItem { expression, alias });
            if !self.consume(&TokenKind::Comma) {
                break;
            }
        }
        self.expect_keyword("from")?;
        let table = self.identifier()?;
        let filter = if self.consume_keyword("where") {
            Some(self.expression()?)
        } else {
            None
        };
        let group_by = if self.consume_keyword("group") {
            self.expect_keyword("by")?;
            self.expression_list()?
        } else {
            Vec::new()
        };
        let having = if self.consume_keyword("having") {
            Some(self.expression()?)
        } else {
            None
        };
        let order_by = if self.consume_keyword("order") {
            self.expect_keyword("by")?;
            self.order_list()?
        } else {
            Vec::new()
        };
        let (limit, offset) = if self.consume_keyword("limit") {
            let first = self.unsigned_integer("LIMIT")?;
            if self.consume(&TokenKind::Comma) {
                (Some(self.unsigned_integer("LIMIT")?), first)
            } else {
                let offset = if self.consume_keyword("offset") {
                    self.unsigned_integer("OFFSET")?
                } else {
                    0
                };
                (Some(first), offset)
            }
        } else {
            (None, 0)
        };
        self.expect_statement_end()?;
        Ok(Select {
            distinct,
            items,
            table,
            filter,
            group_by,
            having,
            order_by,
            limit,
            offset,
        })
    }

    fn expression_list(&mut self) -> Result<Vec<Expr>> {
        let mut expressions = Vec::new();
        loop {
            expressions.push(self.expression()?);
            if !self.consume(&TokenKind::Comma) {
                return Ok(expressions);
            }
        }
    }

    fn order_list(&mut self) -> Result<Vec<OrderBy>> {
        let mut order = Vec::new();
        loop {
            let expression = self.expression()?;
            let ascending = if self.consume_keyword("asc") {
                true
            } else {
                !self.consume_keyword("desc")
            };
            let nulls_first = if self.consume_keyword("nulls") {
                if self.consume_keyword("first") {
                    Some(true)
                } else {
                    self.expect_keyword("last")?;
                    Some(false)
                }
            } else {
                None
            };
            order.push(OrderBy {
                expression,
                ascending,
                nulls_first,
            });
            if !self.consume(&TokenKind::Comma) {
                return Ok(order);
            }
        }
    }

    fn expression(&mut self) -> Result<Expr> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expr> {
        let mut expression = self.parse_and()?;
        while self.consume_keyword("or") {
            expression = binary(expression, BinaryOperator::Or, self.parse_and()?);
        }
        Ok(expression)
    }

    fn parse_and(&mut self) -> Result<Expr> {
        let mut expression = self.parse_not()?;
        while self.consume_keyword("and") {
            expression = binary(expression, BinaryOperator::And, self.parse_not()?);
        }
        Ok(expression)
    }

    fn parse_not(&mut self) -> Result<Expr> {
        if self.consume_keyword("not") {
            Ok(Expr::Unary {
                operator: UnaryOperator::Not,
                expression: Box::new(self.parse_not()?),
            })
        } else {
            self.parse_comparison()
        }
    }

    fn parse_comparison(&mut self) -> Result<Expr> {
        let mut expression = self.parse_additive()?;
        loop {
            let operator = if self.consume(&TokenKind::Equal) {
                Some(BinaryOperator::Equal)
            } else if self.consume(&TokenKind::NotEqual) {
                Some(BinaryOperator::NotEqual)
            } else if self.consume(&TokenKind::Less) {
                Some(BinaryOperator::Less)
            } else if self.consume(&TokenKind::LessEqual) {
                Some(BinaryOperator::LessEqual)
            } else if self.consume(&TokenKind::Greater) {
                Some(BinaryOperator::Greater)
            } else if self.consume(&TokenKind::GreaterEqual) {
                Some(BinaryOperator::GreaterEqual)
            } else {
                None
            };
            if let Some(operator) = operator {
                expression = binary(expression, operator, self.parse_additive()?);
            } else if self.consume_keyword("is") {
                let negated = self.consume_keyword("not");
                self.expect_keyword("null")?;
                expression = Expr::IsNull {
                    expression: Box::new(expression),
                    negated,
                };
            } else {
                return Ok(expression);
            }
        }
    }

    fn parse_additive(&mut self) -> Result<Expr> {
        let mut expression = self.parse_multiplicative()?;
        loop {
            let operator = if self.consume(&TokenKind::Plus) {
                Some(BinaryOperator::Add)
            } else if self.consume(&TokenKind::Minus) {
                Some(BinaryOperator::Subtract)
            } else {
                None
            };
            let Some(operator) = operator else {
                return Ok(expression);
            };
            expression = binary(expression, operator, self.parse_multiplicative()?);
        }
    }

    fn parse_multiplicative(&mut self) -> Result<Expr> {
        let mut expression = self.parse_unary()?;
        loop {
            let operator = if self.consume(&TokenKind::Star) {
                Some(BinaryOperator::Multiply)
            } else if self.consume(&TokenKind::Slash) {
                Some(BinaryOperator::Divide)
            } else if self.consume(&TokenKind::Percent) {
                Some(BinaryOperator::Modulo)
            } else {
                None
            };
            let Some(operator) = operator else {
                return Ok(expression);
            };
            expression = binary(expression, operator, self.parse_unary()?);
        }
    }

    fn parse_unary(&mut self) -> Result<Expr> {
        let operator = if self.consume(&TokenKind::Minus) {
            Some(UnaryOperator::Negate)
        } else if self.consume(&TokenKind::Plus) {
            Some(UnaryOperator::Positive)
        } else {
            None
        };
        if let Some(operator) = operator {
            Ok(Expr::Unary {
                operator,
                expression: Box::new(self.parse_unary()?),
            })
        } else {
            self.parse_primary()
        }
    }

    fn parse_primary(&mut self) -> Result<Expr> {
        if self.consume(&TokenKind::LeftParen) {
            let expression = self.expression()?;
            self.expect(&TokenKind::RightParen, "')'")?;
            return Ok(expression);
        }
        if self.consume(&TokenKind::Star) {
            return Ok(Expr::Wildcard);
        }
        if let TokenKind::Number(number) = self.current().kind.clone() {
            self.advance();
            let value =
                if number.contains(['.', 'e', 'E']) {
                    let value = number
                        .parse::<f64>()
                        .map_err(|_| Error::new(format!("invalid Float64 literal '{number}'")))?;
                    if !value.is_finite() {
                        return Err(Error::new("non-finite Float64 literals are not supported"));
                    }
                    Value::Float64(value)
                } else {
                    Value::Int64(number.parse::<i64>().map_err(|_| {
                        Error::new(format!("Int64 literal out of range: '{number}'"))
                    })?)
                };
            return Ok(Expr::Literal(value));
        }
        if let TokenKind::String(value) = self.current().kind.clone() {
            self.advance();
            return Ok(Expr::Literal(Value::String(value)));
        }
        if self.consume_keyword("null") {
            return Ok(Expr::Literal(Value::Null));
        }
        if self.consume_keyword("true") {
            return Ok(Expr::Literal(Value::Bool(true)));
        }
        if self.consume_keyword("false") {
            return Ok(Expr::Literal(Value::Bool(false)));
        }
        let name = self.identifier()?;
        if self.consume(&TokenKind::LeftParen) {
            let distinct = self.consume_keyword("distinct");
            let mut arguments = Vec::new();
            if !self.check(&TokenKind::RightParen) {
                loop {
                    arguments.push(self.expression()?);
                    if !self.consume(&TokenKind::Comma) {
                        break;
                    }
                }
            }
            self.expect(&TokenKind::RightParen, "')'")?;
            Ok(Expr::Function {
                name: name.to_ascii_lowercase(),
                arguments,
                distinct,
            })
        } else {
            Ok(Expr::Column(name))
        }
    }

    fn identifier(&mut self) -> Result<String> {
        match self.current().kind.clone() {
            TokenKind::Word(name) => {
                self.advance();
                Ok(name)
            }
            _ => Err(self.expected("identifier")),
        }
    }

    fn unsigned_integer(&mut self, context: &str) -> Result<usize> {
        let TokenKind::Number(number) = self.current().kind.clone() else {
            return Err(self.expected(&format!("non-negative integer after {context}")));
        };
        if number.contains(['.', 'e', 'E']) {
            return Err(Error::new(format!("{context} must be an integer")));
        }
        self.advance();
        number
            .parse()
            .map_err(|_| Error::new(format!("{context} is too large")))
    }

    fn next_is_implicit_alias(&self) -> bool {
        let TokenKind::Word(word) = &self.current().kind else {
            return false;
        };
        !matches!(
            word.to_ascii_lowercase().as_str(),
            "from"
                | "where"
                | "group"
                | "having"
                | "order"
                | "limit"
                | "offset"
                | "asc"
                | "desc"
                | "nulls"
                | "and"
                | "or"
                | "is"
        )
    }

    fn expect_statement_end(&self) -> Result<()> {
        if self.check(&TokenKind::Semicolon) || self.is_end() {
            Ok(())
        } else {
            Err(self.expected("end of statement"))
        }
    }

    fn expect_keyword(&mut self, keyword: &str) -> Result<()> {
        if self.consume_keyword(keyword) {
            Ok(())
        } else {
            Err(self.expected(&format!("keyword {keyword}")))
        }
    }

    fn consume_keyword(&mut self, keyword: &str) -> bool {
        if matches!(&self.current().kind, TokenKind::Word(word) if word.eq_ignore_ascii_case(keyword))
        {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: &TokenKind, description: &str) -> Result<()> {
        if self.consume(kind) {
            Ok(())
        } else {
            Err(self.expected(description))
        }
    }

    fn consume(&mut self, kind: &TokenKind) -> bool {
        if self.check(kind) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn check(&self, kind: &TokenKind) -> bool {
        std::mem::discriminant(&self.current().kind) == std::mem::discriminant(kind)
    }

    fn is_end(&self) -> bool {
        self.check(&TokenKind::End)
    }

    fn current(&self) -> &Token {
        &self.tokens[self.index]
    }

    fn advance(&mut self) {
        if !self.is_end() {
            self.index += 1;
        }
    }

    fn expected(&self, expected: &str) -> Error {
        Error::new(format!(
            "SQL error at byte {}: expected {expected}, found {}",
            self.current().offset,
            self.describe_current()
        ))
    }

    fn describe_current(&self) -> String {
        match &self.current().kind {
            TokenKind::Word(value) => format!("'{value}'"),
            TokenKind::Number(value) => format!("number '{value}'"),
            TokenKind::String(_) => "string literal".to_owned(),
            TokenKind::End => "end of input".to_owned(),
            other => format!("{other:?}"),
        }
    }
}

fn binary(left: Expr, operator: BinaryOperator, right: Expr) -> Expr {
    Expr::Binary {
        left: Box::new(left),
        operator,
        right: Box::new(right),
    }
}

pub(crate) fn expression_name(expression: &Expr) -> String {
    match expression {
        Expr::Column(name) => name.clone(),
        Expr::Literal(value) => value.to_string(),
        Expr::Wildcard => "*".to_owned(),
        Expr::Function {
            name,
            arguments,
            distinct,
        } => {
            let distinct = if *distinct { "distinct " } else { "" };
            let arguments = arguments
                .iter()
                .map(expression_name)
                .collect::<Vec<_>>()
                .join(", ");
            format!("{name}({distinct}{arguments})")
        }
        Expr::Binary {
            left,
            operator,
            right,
        } => format!(
            "{} {} {}",
            expression_name(left),
            binary_operator_name(*operator),
            expression_name(right)
        ),
        Expr::Unary {
            operator,
            expression,
        } => format!(
            "{}{}",
            unary_operator_name(*operator),
            expression_name(expression)
        ),
        Expr::IsNull {
            expression,
            negated,
        } => format!(
            "{} is {}null",
            expression_name(expression),
            if *negated { "not " } else { "" }
        ),
    }
}

fn binary_operator_name(operator: BinaryOperator) -> &'static str {
    match operator {
        BinaryOperator::Or => "or",
        BinaryOperator::And => "and",
        BinaryOperator::Equal => "=",
        BinaryOperator::NotEqual => "!=",
        BinaryOperator::Less => "<",
        BinaryOperator::LessEqual => "<=",
        BinaryOperator::Greater => ">",
        BinaryOperator::GreaterEqual => ">=",
        BinaryOperator::Add => "+",
        BinaryOperator::Subtract => "-",
        BinaryOperator::Multiply => "*",
        BinaryOperator::Divide => "/",
        BinaryOperator::Modulo => "%",
    }
}

fn unary_operator_name(operator: UnaryOperator) -> &'static str {
    match operator {
        UnaryOperator::Not => "not ",
        UnaryOperator::Negate => "-",
        UnaryOperator::Positive => "+",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiple_statements_and_quoted_semicolons() {
        let statements = parse(
            "CREATE TABLE t (id Int64, s Nullable(String));\n\
             INSERT INTO t VALUES (1, 'a; b'), (2, NULL); -- comment\n\
             SELECT s, count(*) AS n FROM t WHERE id >= 1 GROUP BY s ORDER BY n DESC LIMIT 2;",
        )
        .unwrap();
        assert_eq!(statements.len(), 3);
        let Statement::Select(select) = &statements[2] else {
            panic!("expected select")
        };
        assert_eq!(select.items.len(), 2);
        assert_eq!(select.group_by.len(), 1);
        assert_eq!(select.order_by.len(), 1);
        assert_eq!(select.limit, Some(2));
    }

    #[test]
    fn reports_malformed_input_with_an_offset() {
        let error = parse("SELECT (id FROM t").unwrap_err();
        assert!(error.message().contains("byte"));
        assert!(error.message().contains("')'"));
    }

    #[test]
    fn rejects_unknown_create_suffix() {
        let error = parse("CREATE TABLE t (id Int64) SOMETHING").unwrap_err();
        assert!(error.message().contains("end of statement"));
    }
}
