use crate::error::{Error, Result};
use crate::storage::{ColumnDef, is_reserved_column_name};
use crate::value::{DataType, Value};

const MAX_PREDICATE_DEPTH: usize = 64;
const MAX_PREDICATE_NODES: usize = 256;
const MAX_EXPRESSION_DEPTH: usize = 64;
const MAX_EXPRESSION_NODES: usize = 256;

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    CreateTable {
        name: String,
        columns: Vec<ColumnDef>,
    },
    Insert {
        table: String,
        rows: Vec<Vec<Value>>,
    },
    Select(Select),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    pub items: Vec<SelectItem>,
    pub table: String,
    pub predicate: Option<Predicate>,
    pub group_by: Vec<String>,
    pub order_by: Vec<OrderBy>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    Wildcard,
    Expression {
        expression: Expression,
        alias: Option<String>,
    },
    Aggregate {
        function: AggregateFunction,
        argument: AggregateArgument,
        alias: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateFunction {
    Count,
    Sum,
    Min,
    Max,
    Avg,
}

impl AggregateFunction {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Count => "COUNT",
            Self::Sum => "SUM",
            Self::Min => "MIN",
            Self::Max => "MAX",
            Self::Avg => "AVG",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_uppercase().as_str() {
            "COUNT" => Some(Self::Count),
            "SUM" => Some(Self::Sum),
            "MIN" => Some(Self::Min),
            "MAX" => Some(Self::Max),
            "AVG" => Some(Self::Avg),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AggregateArgument {
    Wildcard,
    Expression(Expression),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    Comparison {
        left: Expression,
        operator: ComparisonOperator,
        right: Expression,
    },
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expression {
    Column(String),
    Literal(Value),
    Unary {
        operator: UnaryOperator,
        expression: Box<Self>,
    },
    Binary {
        left: Box<Self>,
        operator: BinaryOperator,
        right: Box<Self>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOperator {
    Plus,
    Minus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOperator {
    Add,
    Subtract,
    Multiply,
    Divide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComparisonOperator {
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderBy {
    pub name: String,
    pub descending: bool,
}

/// Parse one or more semicolon-separated SQL statements.
pub fn parse(input: &str) -> Result<Vec<Statement>> {
    let tokens = Lexer::new(input).tokenize()?;
    Parser::new(tokens).parse_script()
}

#[derive(Debug, Clone, PartialEq)]
struct Token {
    kind: TokenKind,
    position: usize,
}

#[derive(Debug, Clone, PartialEq)]
enum TokenKind {
    Identifier(String),
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
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
    End,
}

struct Lexer<'a> {
    input: &'a str,
    position: usize,
}

impl<'a> Lexer<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, position: 0 }
    }

    fn tokenize(mut self) -> Result<Vec<Token>> {
        let mut tokens = Vec::new();
        loop {
            self.skip_ignored();
            let position = self.position;
            let Some(character) = self.current() else {
                tokens.push(Token {
                    kind: TokenKind::End,
                    position,
                });
                return Ok(tokens);
            };

            let kind = match character {
                ',' => {
                    self.advance();
                    TokenKind::Comma
                }
                '(' => {
                    self.advance();
                    TokenKind::LeftParen
                }
                ')' => {
                    self.advance();
                    TokenKind::RightParen
                }
                ';' => {
                    self.advance();
                    TokenKind::Semicolon
                }
                '*' => {
                    self.advance();
                    TokenKind::Star
                }
                '+' => {
                    self.advance();
                    TokenKind::Plus
                }
                '-' => {
                    self.advance();
                    TokenKind::Minus
                }
                '/' => {
                    self.advance();
                    TokenKind::Slash
                }
                '=' => {
                    self.advance();
                    TokenKind::Equal
                }
                '!' => {
                    self.advance();
                    if self.current() != Some('=') {
                        return self.error(position, "expected '=' after '!'");
                    }
                    self.advance();
                    TokenKind::NotEqual
                }
                '<' => {
                    self.advance();
                    match self.current() {
                        Some('=') => {
                            self.advance();
                            TokenKind::LessOrEqual
                        }
                        Some('>') => {
                            self.advance();
                            TokenKind::NotEqual
                        }
                        _ => TokenKind::Less,
                    }
                }
                '>' => {
                    self.advance();
                    if self.current() == Some('=') {
                        self.advance();
                        TokenKind::GreaterOrEqual
                    } else {
                        TokenKind::Greater
                    }
                }
                '\'' => TokenKind::String(self.scan_string(position)?),
                value if value.is_ascii_digit() => TokenKind::Number(self.scan_number()),
                value if value.is_ascii_alphabetic() || value == '_' => {
                    TokenKind::Identifier(self.scan_identifier())
                }
                _ => {
                    return self.error(position, format!("unexpected character '{character}'"));
                }
            };
            tokens.push(Token { kind, position });
        }
    }

    fn current(&self) -> Option<char> {
        self.input[self.position..].chars().next()
    }

    fn advance(&mut self) {
        if let Some(character) = self.current() {
            self.position += character.len_utf8();
        }
    }

    fn skip_ignored(&mut self) {
        loop {
            while self.current().is_some_and(char::is_whitespace) {
                self.advance();
            }
            if self.input[self.position..].starts_with("--") {
                while self.current().is_some_and(|character| character != '\n') {
                    self.advance();
                }
            } else {
                break;
            }
        }
    }

    fn scan_identifier(&mut self) -> String {
        let start = self.position;
        while self
            .current()
            .is_some_and(|value| value.is_ascii_alphanumeric() || value == '_')
        {
            self.advance();
        }
        self.input[start..self.position].to_owned()
    }

    fn scan_number(&mut self) -> String {
        let start = self.position;
        while self.current().is_some_and(|value| value.is_ascii_digit()) {
            self.advance();
        }
        if self.current() == Some('.') {
            self.advance();
            while self.current().is_some_and(|value| value.is_ascii_digit()) {
                self.advance();
            }
        }
        if self
            .current()
            .is_some_and(|value| matches!(value, 'e' | 'E'))
        {
            self.advance();
            if self
                .current()
                .is_some_and(|value| matches!(value, '+' | '-'))
            {
                self.advance();
            }
            while self.current().is_some_and(|value| value.is_ascii_digit()) {
                self.advance();
            }
        }
        self.input[start..self.position].to_owned()
    }

    fn scan_string(&mut self, start: usize) -> Result<String> {
        self.advance();
        let mut value = String::new();
        loop {
            match self.current() {
                None => return self.error(start, "unterminated string literal"),
                Some('\'') => {
                    self.advance();
                    if self.current() == Some('\'') {
                        value.push('\'');
                        self.advance();
                    } else {
                        return Ok(value);
                    }
                }
                Some(character) => {
                    value.push(character);
                    self.advance();
                }
            }
        }
    }

    fn error<T>(&self, position: usize, message: impl Into<String>) -> Result<T> {
        Err(Error::Sql {
            position,
            message: message.into(),
        })
    }
}

struct Parser {
    tokens: Vec<Token>,
    current: usize,
    predicate_depth: usize,
    predicate_nodes: usize,
    expression_depth: usize,
    expression_nodes: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self {
            tokens,
            current: 0,
            predicate_depth: 0,
            predicate_nodes: 0,
            expression_depth: 0,
            expression_nodes: 0,
        }
    }

    fn parse_script(mut self) -> Result<Vec<Statement>> {
        let mut statements = Vec::new();
        while self.eat(&TokenKind::Semicolon) {}
        while !self.at(&TokenKind::End) {
            statements.push(self.parse_statement()?);
            if !self.eat(&TokenKind::Semicolon) && !self.at(&TokenKind::End) {
                return self.error("expected ';' between statements");
            }
            while self.eat(&TokenKind::Semicolon) {}
        }
        if statements.is_empty() {
            return self.error("expected a SQL statement");
        }
        Ok(statements)
    }

    fn parse_statement(&mut self) -> Result<Statement> {
        if self.eat_keyword("CREATE") {
            self.parse_create()
        } else if self.eat_keyword("INSERT") {
            self.parse_insert()
        } else if self.eat_keyword("SELECT") {
            self.parse_select().map(Statement::Select)
        } else {
            self.error("expected CREATE, INSERT, or SELECT")
        }
    }

    fn parse_create(&mut self) -> Result<Statement> {
        self.expect_keyword("TABLE")?;
        let name = self.expect_identifier("table name")?;
        self.expect(&TokenKind::LeftParen, "'(' after table name")?;
        let mut columns = Vec::new();
        loop {
            let column_name = self.expect_identifier("column name")?;
            if is_reserved_column_name(&column_name) {
                return Err(Error::ReservedIdentifier {
                    identifier: column_name,
                    context: "column name".to_owned(),
                });
            }
            let position = self.position();
            let type_name = self.expect_identifier("column type")?;
            let data_type = DataType::parse(&type_name).ok_or_else(|| Error::Sql {
                position,
                message: format!(
                    "unknown type '{type_name}'; expected Int64, Float64, Bool, or String"
                ),
            })?;
            columns.push(ColumnDef {
                name: column_name,
                data_type,
            });
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RightParen, "')' after column definitions")?;
        Ok(Statement::CreateTable { name, columns })
    }

    fn parse_insert(&mut self) -> Result<Statement> {
        self.expect_keyword("INTO")?;
        let table = self.expect_identifier("table name")?;
        self.expect_keyword("VALUES")?;
        let mut rows = Vec::new();
        loop {
            self.expect(&TokenKind::LeftParen, "'(' before row values")?;
            let mut row = Vec::new();
            if !self.at(&TokenKind::RightParen) {
                loop {
                    row.push(self.parse_literal()?);
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                }
            }
            self.expect(&TokenKind::RightParen, "')' after row values")?;
            rows.push(row);
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        Ok(Statement::Insert { table, rows })
    }

    fn parse_select(&mut self) -> Result<Select> {
        let mut items = Vec::new();
        loop {
            items.push(self.parse_select_item()?);
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect_keyword("FROM")?;
        let table = self.expect_identifier("table name")?;

        let predicate = if self.eat_keyword("WHERE") {
            self.predicate_depth = 0;
            self.predicate_nodes = 0;
            Some(self.parse_or_predicate()?)
        } else {
            None
        };

        let mut group_by = Vec::new();
        if self.eat_keyword("GROUP") {
            self.expect_keyword("BY")?;
            loop {
                group_by.push(self.expect_identifier("GROUP BY column")?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }

        let mut order_by = Vec::new();
        if self.eat_keyword("ORDER") {
            self.expect_keyword("BY")?;
            loop {
                let name = self.expect_identifier("ORDER BY output column or alias")?;
                let descending = if self.eat_keyword("DESC") {
                    true
                } else {
                    self.eat_keyword("ASC");
                    false
                };
                order_by.push(OrderBy { name, descending });
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }

        let limit = if self.eat_keyword("LIMIT") {
            let position = self.position();
            let number = self.take_number().ok_or_else(|| Error::Sql {
                position,
                message: "expected a non-negative integer after LIMIT".to_owned(),
            })?;
            Some(number.parse::<usize>().map_err(|_| Error::Sql {
                position,
                message: format!("invalid LIMIT '{number}'"),
            })?)
        } else {
            None
        };

        Ok(Select {
            items,
            table,
            predicate,
            group_by,
            order_by,
            limit,
        })
    }

    fn parse_select_item(&mut self) -> Result<SelectItem> {
        if self.eat(&TokenKind::Star) {
            return Ok(SelectItem::Wildcard);
        }

        if let TokenKind::Identifier(name) = self.peek().clone()
            && self
                .tokens
                .get(self.current + 1)
                .is_some_and(|token| token.kind == TokenKind::LeftParen)
        {
            let position = self.position();
            self.current += 2;
            let function = AggregateFunction::parse(&name).ok_or_else(|| Error::Sql {
                position,
                message: format!("unknown aggregate function '{name}'"),
            })?;
            let argument = if self.eat(&TokenKind::Star) {
                AggregateArgument::Wildcard
            } else {
                AggregateArgument::Expression(self.parse_expression_root()?)
            };
            self.expect(&TokenKind::RightParen, "')' after aggregate argument")?;
            let alias = self.parse_alias()?;
            Ok(SelectItem::Aggregate {
                function,
                argument,
                alias,
            })
        } else {
            let expression = self.parse_expression_root()?;
            let alias = self.parse_alias()?;
            Ok(SelectItem::Expression { expression, alias })
        }
    }

    fn parse_alias(&mut self) -> Result<Option<String>> {
        if self.eat_keyword("AS") {
            self.expect_identifier("alias").map(Some)
        } else {
            Ok(None)
        }
    }

    fn parse_or_predicate(&mut self) -> Result<Predicate> {
        let mut predicate = self.parse_and_predicate()?;
        while self.eat_keyword("OR") {
            let right = self.parse_and_predicate()?;
            self.record_predicate_node()?;
            predicate = Predicate::Or(Box::new(predicate), Box::new(right));
        }
        Ok(predicate)
    }

    fn parse_and_predicate(&mut self) -> Result<Predicate> {
        let mut predicate = self.parse_predicate_atom()?;
        while self.eat_keyword("AND") {
            let right = self.parse_predicate_atom()?;
            self.record_predicate_node()?;
            predicate = Predicate::And(Box::new(predicate), Box::new(right));
        }
        Ok(predicate)
    }

    fn parse_predicate_atom(&mut self) -> Result<Predicate> {
        let checkpoint = self.current;
        let predicate_nodes = self.predicate_nodes;
        let comparison = self.parse_comparison_predicate();
        if comparison.is_ok() {
            return comparison;
        }
        self.current = checkpoint;
        self.predicate_nodes = predicate_nodes;

        if self.eat(&TokenKind::LeftParen) {
            if self.predicate_depth >= MAX_PREDICATE_DEPTH {
                return self.error(format!(
                    "predicate nesting exceeds limit of {MAX_PREDICATE_DEPTH}"
                ));
            }
            self.predicate_depth += 1;
            let predicate = self.parse_or_predicate();
            self.predicate_depth -= 1;
            let predicate = predicate?;
            self.expect(&TokenKind::RightParen, "right parenthesis after predicate")?;
            return Ok(predicate);
        }

        comparison
    }

    fn parse_comparison_predicate(&mut self) -> Result<Predicate> {
        let left = self.parse_expression_root()?;
        let operator = match self.peek() {
            TokenKind::Equal => ComparisonOperator::Equal,
            TokenKind::NotEqual => ComparisonOperator::NotEqual,
            TokenKind::Less => ComparisonOperator::Less,
            TokenKind::LessOrEqual => ComparisonOperator::LessOrEqual,
            TokenKind::Greater => ComparisonOperator::Greater,
            TokenKind::GreaterOrEqual => ComparisonOperator::GreaterOrEqual,
            _ => return self.error("expected comparison operator (=, !=, <, <=, >, or >=)"),
        };
        self.current += 1;
        let right = self.parse_expression_root()?;
        self.record_predicate_node()?;
        Ok(Predicate::Comparison {
            left,
            operator,
            right,
        })
    }

    fn record_predicate_node(&mut self) -> Result<()> {
        if self.predicate_nodes >= MAX_PREDICATE_NODES {
            return self.error(format!(
                "predicate is too complex; maximum {MAX_PREDICATE_NODES} expression nodes"
            ));
        }
        self.predicate_nodes += 1;
        Ok(())
    }

    fn parse_expression_root(&mut self) -> Result<Expression> {
        self.expression_depth = 0;
        self.expression_nodes = 0;
        self.parse_additive_expression()
    }

    fn parse_additive_expression(&mut self) -> Result<Expression> {
        let mut expression = self.parse_multiplicative_expression()?;
        loop {
            let operator = if self.eat(&TokenKind::Plus) {
                BinaryOperator::Add
            } else if self.eat(&TokenKind::Minus) {
                BinaryOperator::Subtract
            } else {
                break;
            };
            let right = self.parse_multiplicative_expression()?;
            self.record_expression_node()?;
            expression = Expression::Binary {
                left: Box::new(expression),
                operator,
                right: Box::new(right),
            };
        }
        Ok(expression)
    }

    fn parse_multiplicative_expression(&mut self) -> Result<Expression> {
        let mut expression = self.parse_unary_expression()?;
        loop {
            let operator = if self.eat(&TokenKind::Star) {
                BinaryOperator::Multiply
            } else if self.eat(&TokenKind::Slash) {
                BinaryOperator::Divide
            } else {
                break;
            };
            let right = self.parse_unary_expression()?;
            self.record_expression_node()?;
            expression = Expression::Binary {
                left: Box::new(expression),
                operator,
                right: Box::new(right),
            };
        }
        Ok(expression)
    }

    fn parse_unary_expression(&mut self) -> Result<Expression> {
        let operator = if self.eat(&TokenKind::Plus) {
            Some(UnaryOperator::Plus)
        } else if self.eat(&TokenKind::Minus) {
            Some(UnaryOperator::Minus)
        } else {
            None
        };
        let Some(operator) = operator else {
            return self.parse_primary_expression();
        };

        // i64::MIN has no positive Int64 representation for the child node.
        if operator == UnaryOperator::Minus
            && let TokenKind::Number(number) = self.peek().clone()
            && !number.contains(['.', 'e', 'E'])
            && number.parse::<i64>().is_err()
            && let Ok(value) = format!("-{number}").parse::<i64>()
        {
            self.current += 1;
            self.record_expression_node()?;
            return Ok(Expression::Literal(Value::Int64(value)));
        }

        if self.expression_depth >= MAX_EXPRESSION_DEPTH {
            return self.error(format!(
                "expression nesting exceeds limit of {MAX_EXPRESSION_DEPTH}"
            ));
        }
        self.expression_depth += 1;
        let expression = self.parse_unary_expression();
        self.expression_depth -= 1;
        let expression = expression?;
        self.record_expression_node()?;
        Ok(Expression::Unary {
            operator,
            expression: Box::new(expression),
        })
    }

    fn parse_primary_expression(&mut self) -> Result<Expression> {
        if self.eat(&TokenKind::LeftParen) {
            if self.expression_depth >= MAX_EXPRESSION_DEPTH {
                return self.error(format!(
                    "expression nesting exceeds limit of {MAX_EXPRESSION_DEPTH}"
                ));
            }
            self.expression_depth += 1;
            let expression = self.parse_additive_expression();
            self.expression_depth -= 1;
            let expression = expression?;
            self.expect(&TokenKind::RightParen, "')' after expression")?;
            return Ok(expression);
        }

        let expression = match self.peek().clone() {
            TokenKind::String(value) => {
                self.current += 1;
                Expression::Literal(Value::String(value))
            }
            TokenKind::Number(number) => {
                let position = self.position();
                self.current += 1;
                Expression::Literal(parse_number(&number, position)?)
            }
            TokenKind::Identifier(value) if value.eq_ignore_ascii_case("TRUE") => {
                self.current += 1;
                Expression::Literal(Value::Bool(true))
            }
            TokenKind::Identifier(value) if value.eq_ignore_ascii_case("FALSE") => {
                self.current += 1;
                Expression::Literal(Value::Bool(false))
            }
            TokenKind::Identifier(name) => {
                self.current += 1;
                Expression::Column(name)
            }
            _ => return self.error("expected a column, literal, or parenthesized expression"),
        };
        self.record_expression_node()?;
        Ok(expression)
    }

    fn record_expression_node(&mut self) -> Result<()> {
        if self.expression_nodes >= MAX_EXPRESSION_NODES {
            return self.error(format!(
                "expression is too complex; maximum {MAX_EXPRESSION_NODES} nodes"
            ));
        }
        self.expression_nodes += 1;
        Ok(())
    }

    fn parse_literal(&mut self) -> Result<Value> {
        if let TokenKind::String(value) = self.peek().clone() {
            self.current += 1;
            return Ok(Value::String(value));
        }

        let negative = self.eat(&TokenKind::Minus);
        if let Some(number) = self.take_number() {
            let signed = if negative {
                format!("-{number}")
            } else {
                number
            };
            if signed.contains(['.', 'e', 'E']) {
                let value = signed.parse::<f64>().map_err(|_| Error::Sql {
                    position: self.position(),
                    message: format!("invalid Float64 literal '{signed}'"),
                })?;
                if !value.is_finite() {
                    return self.error("Float64 literal must be finite");
                }
                return Ok(Value::Float64(value));
            }
            return signed
                .parse::<i64>()
                .map(Value::Int64)
                .map_err(|_| Error::Sql {
                    position: self.position(),
                    message: format!("invalid Int64 literal '{signed}'"),
                });
        }
        if negative {
            return self.error("expected a number after '-'");
        }

        if self.eat_keyword("TRUE") {
            Ok(Value::Bool(true))
        } else if self.eat_keyword("FALSE") {
            Ok(Value::Bool(false))
        } else {
            self.error("expected an Int64, Float64, Bool, or String literal")
        }
    }

    fn expect_keyword(&mut self, expected: &str) -> Result<()> {
        if self.eat_keyword(expected) {
            Ok(())
        } else {
            self.error(format!("expected keyword {expected}"))
        }
    }

    fn eat_keyword(&mut self, expected: &str) -> bool {
        if matches!(self.peek(), TokenKind::Identifier(value) if value.eq_ignore_ascii_case(expected))
        {
            self.current += 1;
            true
        } else {
            false
        }
    }

    fn expect_identifier(&mut self, description: &str) -> Result<String> {
        if let TokenKind::Identifier(value) = self.peek().clone() {
            self.current += 1;
            Ok(value)
        } else {
            self.error(format!("expected {description}"))
        }
    }

    fn take_number(&mut self) -> Option<String> {
        if let TokenKind::Number(value) = self.peek().clone() {
            self.current += 1;
            Some(value)
        } else {
            None
        }
    }

    fn expect(&mut self, expected: &TokenKind, description: &str) -> Result<()> {
        if self.eat(expected) {
            Ok(())
        } else {
            self.error(format!("expected {description}"))
        }
    }

    fn eat(&mut self, expected: &TokenKind) -> bool {
        if self.at(expected) {
            self.current += 1;
            true
        } else {
            false
        }
    }

    fn at(&self, expected: &TokenKind) -> bool {
        self.peek() == expected
    }

    fn peek(&self) -> &TokenKind {
        &self.tokens[self.current].kind
    }

    fn position(&self) -> usize {
        self.tokens[self.current].position
    }

    fn error<T>(&self, message: impl Into<String>) -> Result<T> {
        Err(Error::Sql {
            position: self.position(),
            message: message.into(),
        })
    }
}

fn parse_number(number: &str, position: usize) -> Result<Value> {
    if number.contains(['.', 'e', 'E']) {
        let value = number.parse::<f64>().map_err(|_| Error::Sql {
            position,
            message: format!("invalid Float64 literal '{number}'"),
        })?;
        if !value.is_finite() {
            return Err(Error::Sql {
                position,
                message: "Float64 literal must be finite".to_owned(),
            });
        }
        Ok(Value::Float64(value))
    } else {
        number
            .parse::<i64>()
            .map(Value::Int64)
            .map_err(|_| Error::Sql {
                position,
                message: format!("invalid Int64 literal '{number}'"),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_complete_select_shape() {
        let statements = parse(
            "SELECT region, SUM(amount) AS total FROM sales \
             WHERE active = true AND amount >= 2.5 \
             GROUP BY region ORDER BY total DESC LIMIT 3;",
        )
        .expect("valid SQL");

        let Statement::Select(select) = &statements[0] else {
            panic!("expected select");
        };
        assert_eq!(select.items.len(), 2);
        assert_eq!(select.group_by, ["region"]);
        assert_eq!(select.order_by[0].name, "total");
        assert!(select.order_by[0].descending);
        assert_eq!(select.limit, Some(3));
    }

    #[test]
    fn parses_escaped_strings_and_multiple_rows() {
        let statements =
            parse("INSERT INTO notes VALUES (1, 'it''s good'), (2, 'ok')").expect("valid insert");
        let Statement::Insert { rows, .. } = &statements[0] else {
            panic!("expected insert");
        };
        assert_eq!(rows[0][1], Value::String("it's good".to_owned()));
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn reports_syntax_position() {
        let error = parse("SELECT id FROM things WHERE id ! 2").expect_err("bad operator");
        assert!(matches!(error, Error::Sql { position: 31, .. }));
    }

    #[test]
    fn rejects_fifty_thousand_nested_predicates() {
        let depth = 50_000;
        let sql = format!(
            "SELECT id FROM things WHERE {}id = 1{}",
            "(".repeat(depth),
            ")".repeat(depth)
        );

        let error = parse(&sql).expect_err("nesting limit should reject query");
        assert!(matches!(
            error,
            Error::Sql { message, .. } if message.contains("predicate nesting exceeds limit of 64")
        ));
    }

    #[test]
    fn rejects_fifty_thousand_flat_predicate_terms() {
        let predicate = vec!["id = 1"; 50_000].join(" OR ");
        let sql = format!("SELECT id FROM things WHERE {predicate}");

        let error = parse(&sql).expect_err("node limit should reject query");
        assert!(matches!(
            error,
            Error::Sql { message, .. }
                if message.contains("predicate is too complex; maximum 256 expression nodes")
        ));
    }

    #[test]
    fn parses_numeric_expression_precedence_and_unary_signs() {
        let statements =
            parse("SELECT a + b * 2, SUM(-a / +2) FROM valueset").expect("expressions are valid");
        let Statement::Select(select) = &statements[0] else {
            panic!("expected select");
        };
        let SelectItem::Expression {
            expression:
                Expression::Binary {
                    operator: BinaryOperator::Add,
                    right,
                    ..
                },
            ..
        } = &select.items[0]
        else {
            panic!("expected additive projection");
        };
        assert!(matches!(
            right.as_ref(),
            Expression::Binary {
                operator: BinaryOperator::Multiply,
                ..
            }
        ));

        let SelectItem::Aggregate {
            argument: AggregateArgument::Expression(expression),
            ..
        } = &select.items[1]
        else {
            panic!("expected aggregate expression");
        };
        assert!(matches!(
            expression,
            Expression::Binary {
                operator: BinaryOperator::Divide,
                ..
            }
        ));
    }

    #[test]
    fn rejects_pathological_numeric_expressions() {
        let flat = vec!["1"; 50_000].join(" + ");
        let error = parse(&format!("SELECT {flat} FROM valueset"))
            .expect_err("expression node limit should reject query");
        assert!(matches!(
            error,
            Error::Sql { message, .. }
                if message.contains("expression is too complex; maximum 256 nodes")
        ));

        let nested = format!(
            "SELECT {}1{} FROM valueset",
            "(".repeat(50_000),
            ")".repeat(50_000)
        );
        let error = parse(&nested).expect_err("expression nesting limit should reject query");
        assert!(matches!(
            error,
            Error::Sql { message, .. }
                if message.contains("expression nesting exceeds limit of 64")
        ));
    }
}
