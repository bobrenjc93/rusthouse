use crate::error::{Error, Result};
use crate::storage::{ColumnDef, is_reserved_column_name};
use crate::value::{DataType, Value};

const MAX_PREDICATE_DEPTH: usize = 64;
const MAX_PREDICATE_NODES: usize = 256;
pub(crate) const MAX_JOIN_KEYS: usize = 64;

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
    pub table_alias: Option<String>,
    pub joins: Vec<Join>,
    pub predicate: Option<Predicate>,
    pub group_by: Vec<String>,
    pub order_by: Vec<OrderBy>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    Wildcard,
    QualifiedWildcard {
        qualifier: String,
    },
    Column {
        name: String,
        alias: Option<String>,
    },
    Aggregate {
        function: AggregateFunction,
        argument: AggregateArgument,
        alias: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    pub kind: JoinKind,
    pub table: String,
    pub alias: Option<String>,
    pub conditions: Vec<JoinCondition>,
    pub predicate: Option<Predicate>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    LeftSemi,
    LeftAnti,
}

impl JoinKind {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Inner => "INNER JOIN",
            Self::Left => "LEFT JOIN",
            Self::LeftSemi => "LEFT SEMI JOIN",
            Self::LeftAnti => "LEFT ANTI JOIN",
        }
    }

    #[must_use]
    pub fn is_left_filter(self) -> bool {
        matches!(self, Self::LeftSemi | Self::LeftAnti)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinCondition {
    pub left: String,
    pub right: String,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AggregateArgument {
    Wildcard,
    Column(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    Comparison {
        left: Operand,
        operator: ComparisonOperator,
        right: Operand,
    },
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Operand {
    Column(String),
    Literal(Value),
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
    Dot,
    LeftParen,
    RightParen,
    Semicolon,
    Star,
    Minus,
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
                '.' => {
                    self.advance();
                    TokenKind::Dot
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
                '-' => {
                    self.advance();
                    TokenKind::Minus
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
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self {
            tokens,
            current: 0,
            predicate_depth: 0,
            predicate_nodes: 0,
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
            let (type_name, nullable) = if type_name.eq_ignore_ascii_case("NULLABLE") {
                self.expect(&TokenKind::LeftParen, "'(' after Nullable")?;
                let nested = self.expect_identifier("type inside Nullable")?;
                self.expect(&TokenKind::RightParen, "')' after Nullable type")?;
                (nested, true)
            } else {
                (type_name, false)
            };
            let data_type = DataType::parse(&type_name).ok_or_else(|| Error::Sql {
                position,
                message: format!(
                    "unknown type '{type_name}'; expected Int64, Float64, Bool, or String"
                ),
            })?;
            columns.push(ColumnDef {
                name: column_name,
                data_type,
                nullable,
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
        let table_alias =
            self.parse_table_alias(&["INNER", "LEFT", "WHERE", "GROUP", "ORDER", "LIMIT"])?;

        let mut joins = Vec::new();
        let join_kind = if self.eat_keyword("INNER") {
            Some(JoinKind::Inner)
        } else if self.eat_keyword("LEFT") {
            if self.eat_keyword("SEMI") {
                Some(JoinKind::LeftSemi)
            } else if self.eat_keyword("ANTI") {
                Some(JoinKind::LeftAnti)
            } else {
                self.eat_keyword("OUTER");
                Some(JoinKind::Left)
            }
        } else {
            None
        };
        if let Some(kind) = join_kind {
            self.expect_keyword("JOIN")?;
            let join_table = self.expect_identifier("joined table name")?;
            let alias = self.parse_table_alias(&["ON"])?;
            self.expect_keyword("ON")?;
            let mut conditions = Vec::new();
            let mut residual = Vec::new();
            loop {
                let left = self.parse_operand()?;
                let operator = self.parse_comparison_operator()?;
                let right = self.parse_operand()?;
                match (&left, operator, &right) {
                    (Operand::Column(left), ComparisonOperator::Equal, Operand::Column(right)) => {
                        conditions.push(JoinCondition {
                            left: left.clone(),
                            right: right.clone(),
                        })
                    }
                    _ => residual.push(Predicate::Comparison {
                        left,
                        operator,
                        right,
                    }),
                }
                if !self.eat_keyword("AND") {
                    break;
                }
                if conditions.len() + residual.len() >= MAX_PREDICATE_NODES {
                    return self.error(format!("{} condition is too complex", kind.name()));
                }
            }
            let predicate = residual
                .into_iter()
                .reduce(|left, right| Predicate::And(Box::new(left), Box::new(right)));
            joins.push(Join {
                kind,
                table: join_table,
                alias,
                conditions,
                predicate,
            });
            if self.at_keyword("INNER") || self.at_keyword("LEFT") || self.at_keyword("JOIN") {
                return self.error("only one JOIN is supported per SELECT");
            }
        }

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
                group_by.push(self.parse_column_name("GROUP BY column")?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }

        let mut order_by = Vec::new();
        if self.eat_keyword("ORDER") {
            self.expect_keyword("BY")?;
            loop {
                let name = self.parse_column_name("ORDER BY output column or alias")?;
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
            table_alias,
            joins,
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

        let position = self.position();
        let first = self.expect_identifier("column or aggregate function")?;
        if self.eat(&TokenKind::Dot) {
            if self.eat(&TokenKind::Star) {
                return Ok(SelectItem::QualifiedWildcard { qualifier: first });
            }
            let name = format!(
                "{first}.{}",
                self.expect_identifier("column name after '.'")?
            );
            let alias = self.parse_alias()?;
            return Ok(SelectItem::Column { name, alias });
        }
        let name = first;
        if self.eat(&TokenKind::LeftParen) {
            let function = AggregateFunction::parse(&name).ok_or_else(|| Error::Sql {
                position,
                message: format!("unknown aggregate function '{name}'"),
            })?;
            let argument = if self.eat(&TokenKind::Star) {
                AggregateArgument::Wildcard
            } else {
                AggregateArgument::Column(self.parse_column_name("aggregate column")?)
            };
            self.expect(&TokenKind::RightParen, "')' after aggregate argument")?;
            let alias = self.parse_alias()?;
            Ok(SelectItem::Aggregate {
                function,
                argument,
                alias,
            })
        } else {
            let alias = self.parse_alias()?;
            Ok(SelectItem::Column { name, alias })
        }
    }

    fn parse_alias(&mut self) -> Result<Option<String>> {
        if self.eat_keyword("AS") {
            self.expect_identifier("alias").map(Some)
        } else {
            Ok(None)
        }
    }

    fn parse_table_alias(&mut self, terminators: &[&str]) -> Result<Option<String>> {
        if self.eat_keyword("AS") {
            return self.expect_identifier("table alias").map(Some);
        }
        if matches!(self.peek(), TokenKind::Identifier(value)
            if !terminators.iter().any(|keyword| value.eq_ignore_ascii_case(keyword)))
        {
            return self.expect_identifier("table alias").map(Some);
        }
        Ok(None)
    }

    fn parse_column_name(&mut self, description: &str) -> Result<String> {
        let qualifier_or_name = self.expect_identifier(description)?;
        if self.eat(&TokenKind::Dot) {
            let name = self.expect_identifier("column name after '.'")?;
            Ok(format!("{qualifier_or_name}.{name}"))
        } else {
            Ok(qualifier_or_name)
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

        let left = self.parse_operand()?;
        let operator = self.parse_comparison_operator()?;
        let right = self.parse_operand()?;
        self.record_predicate_node()?;
        Ok(Predicate::Comparison {
            left,
            operator,
            right,
        })
    }

    fn parse_comparison_operator(&mut self) -> Result<ComparisonOperator> {
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
        Ok(operator)
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

    fn parse_operand(&mut self) -> Result<Operand> {
        match self.peek() {
            TokenKind::String(_) | TokenKind::Number(_) | TokenKind::Minus => {
                self.parse_literal().map(Operand::Literal)
            }
            TokenKind::Identifier(value)
                if (value.eq_ignore_ascii_case("TRUE")
                    || value.eq_ignore_ascii_case("FALSE")
                    || value.eq_ignore_ascii_case("NULL"))
                    && !self.next_is(&TokenKind::Dot) =>
            {
                self.parse_literal().map(Operand::Literal)
            }
            TokenKind::Identifier(_) => self
                .parse_column_name("column or literal")
                .map(Operand::Column),
            _ => self.error("expected column or literal"),
        }
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
        } else if self.eat_keyword("NULL") {
            Ok(Value::Null)
        } else {
            self.error("expected an Int64, Float64, Bool, String, or NULL literal")
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

    fn at_keyword(&self, expected: &str) -> bool {
        matches!(self.peek(), TokenKind::Identifier(value) if value.eq_ignore_ascii_case(expected))
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

    fn next_is(&self, expected: &TokenKind) -> bool {
        self.tokens
            .get(self.current + 1)
            .is_some_and(|token| &token.kind == expected)
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
    fn parses_one_aliased_composite_inner_join() {
        let statements = parse(
            "SELECT a.*, SUM(b.amount) AS total
             FROM accounts AS a
             INNER JOIN bills b ON a.id = b.account_id AND a.region = b.region
             GROUP BY a.id, a.region
             ORDER BY a.id;",
        )
        .expect("valid join");

        let Statement::Select(select) = &statements[0] else {
            panic!("expected select");
        };
        assert_eq!(select.table, "accounts");
        assert_eq!(select.table_alias.as_deref(), Some("a"));
        assert_eq!(select.joins.len(), 1);
        assert_eq!(select.joins[0].alias.as_deref(), Some("b"));
        assert_eq!(select.joins[0].conditions.len(), 2);
        assert_eq!(select.group_by, ["a.id", "a.region"]);
        assert_eq!(select.order_by[0].name, "a.id");

        let error = parse(
            "SELECT a.id FROM a INNER JOIN b ON a.id = b.id
             INNER JOIN c ON b.id = c.id",
        )
        .expect_err("only one join is supported");
        assert!(matches!(
            error,
            Error::Sql { message, .. } if message.contains("only one JOIN")
        ));
    }

    #[test]
    fn parses_left_outer_join_nullable_types_and_null_literals() {
        let statements = parse(
            "CREATE TABLE right_rows (id Nullable(Int64));
             INSERT INTO right_rows VALUES (NULL);
             SELECT l.id, r.id
             FROM left_rows l LEFT OUTER JOIN right_rows r
               ON l.id = r.id AND r.id > 0;",
        )
        .expect("valid left outer join batch");

        let Statement::CreateTable { columns, .. } = &statements[0] else {
            panic!("expected create table");
        };
        assert!(columns[0].nullable);
        let Statement::Insert { rows, .. } = &statements[1] else {
            panic!("expected insert");
        };
        assert_eq!(rows[0], [Value::Null]);
        let Statement::Select(select) = &statements[2] else {
            panic!("expected select");
        };
        assert_eq!(select.joins[0].kind, JoinKind::Left);
        assert_eq!(select.joins[0].conditions.len(), 1);
        assert!(select.joins[0].predicate.is_some());
    }

    #[test]
    fn parses_left_semi_and_left_anti_joins() {
        let statements = parse(
            "SELECT l.* FROM left_rows l LEFT SEMI JOIN right_rows r ON l.id = r.id;
             SELECT l.id FROM left_rows l LEFT ANTI JOIN right_rows r
               ON l.id = r.id AND r.enabled = true;",
        )
        .expect("valid filtering joins");

        let Statement::Select(semi) = &statements[0] else {
            panic!("expected SELECT");
        };
        assert_eq!(semi.joins[0].kind, JoinKind::LeftSemi);
        let Statement::Select(anti) = &statements[1] else {
            panic!("expected SELECT");
        };
        assert_eq!(anti.joins[0].kind, JoinKind::LeftAnti);
        assert!(anti.joins[0].predicate.is_some());
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
}
