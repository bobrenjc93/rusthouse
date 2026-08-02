use crate::{DataType, DatabaseError, LimitKind, Value};

pub(crate) const MAX_SAFE_EXPRESSION_DEPTH: usize = 64;
pub(crate) const MAX_SAFE_EXPRESSION_NODES: usize = 256;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Statement {
    CreateTable {
        name: Identifier,
        if_not_exists: bool,
        columns: Vec<CreateColumn>,
    },
    Insert {
        table: Identifier,
        columns: Option<Vec<Identifier>>,
        rows: Vec<Vec<Expr>>,
    },
    Select(Select),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Identifier {
    pub(crate) value: String,
    pub(crate) quoted: bool,
}

impl Identifier {
    pub(crate) fn unquoted(value: String) -> Self {
        Self {
            value,
            quoted: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CreateColumn {
    pub(crate) name: Identifier,
    pub(crate) data_type: DataType,
}

fn validate_expression_depth(statement: &Statement, max_depth: usize) -> Result<(), DatabaseError> {
    let mut stack = Vec::new();
    match statement {
        Statement::CreateTable { .. } => {}
        Statement::Insert { rows, .. } => {
            for expression in rows.iter().flatten() {
                stack.push((expression, 1_usize));
            }
        }
        Statement::Select(select) => {
            for item in &select.items {
                if let SelectItem::Expr { expr, .. } = item {
                    stack.push((expr, 1));
                }
            }
            if let Some(filter) = &select.filter {
                stack.push((filter, 1));
            }
            stack.extend(select.group_by.iter().map(|expr| (expr, 1)));
            stack.extend(select.order_by.iter().map(|order| (&order.expr, 1)));
        }
    }

    while let Some((expression, depth)) = stack.pop() {
        if depth > max_depth {
            return Err(DatabaseError::LimitExceeded {
                kind: LimitKind::ExpressionDepth,
                limit: max_depth,
                actual: depth,
            });
        }
        match expression {
            Expr::Aggregate { argument, .. } => {
                if let Some(argument) = argument {
                    stack.push((argument, depth + 1));
                }
            }
            Expr::Binary { left, right, .. } => {
                stack.push((left, depth + 1));
                stack.push((right, depth + 1));
            }
            Expr::Unary { expr, .. } => stack.push((expr, depth + 1)),
            Expr::Literal(_) | Expr::Column(_) => {}
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Select {
    pub(crate) items: Vec<SelectItem>,
    pub(crate) from: Option<Identifier>,
    pub(crate) filter: Option<Expr>,
    pub(crate) group_by: Vec<Expr>,
    pub(crate) order_by: Vec<OrderBy>,
    pub(crate) limit: Option<usize>,
    pub(crate) offset: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SelectItem {
    Wildcard,
    Expr {
        expr: Expr,
        alias: Option<Identifier>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OrderBy {
    pub(crate) expr: Expr,
    pub(crate) descending: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AggregateFunction {
    Count,
    Sum,
    Min,
    Max,
    Avg,
}

impl AggregateFunction {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::Sum => "sum",
            Self::Min => "min",
            Self::Max => "max",
            Self::Avg => "avg",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinaryOperator {
    Or,
    And,
    Eq,
    NotEq,
    Less,
    LessEq,
    Greater,
    GreaterEq,
    Add,
    Subtract,
    Multiply,
    Divide,
    Modulo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnaryOperator {
    Not,
    Negate,
    Positive,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Expr {
    Literal(Value),
    Column(ColumnReference),
    Aggregate {
        function: AggregateFunction,
        argument: Option<Box<Expr>>,
    },
    Binary {
        left: Box<Expr>,
        operator: BinaryOperator,
        right: Box<Expr>,
    },
    Unary {
        operator: UnaryOperator,
        expr: Box<Expr>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ColumnReference {
    pub(crate) qualifier: Option<Identifier>,
    pub(crate) name: Identifier,
}

impl ColumnReference {
    pub(crate) fn unqualified(name: String, quoted: bool) -> Self {
        Self {
            qualifier: None,
            name: Identifier {
                value: name,
                quoted,
            },
        }
    }

    pub(crate) fn label(&self) -> String {
        self.qualifier.as_ref().map_or_else(
            || self.name.value.clone(),
            |table| format!("{}.{}", table.value, self.name.value),
        )
    }
}

impl Expr {
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Literal(value) => match value {
                Value::String(value) => format!("'{value}'"),
                _ => value.to_string(),
            },
            Self::Column(reference) => reference.label(),
            Self::Aggregate { function, argument } => match argument {
                Some(argument) => format!("{}({})", function.name(), argument.label()),
                None => format!("{}(*)", function.name()),
            },
            Self::Binary {
                left,
                operator,
                right,
            } => {
                let op = match operator {
                    BinaryOperator::Or => "OR",
                    BinaryOperator::And => "AND",
                    BinaryOperator::Eq => "=",
                    BinaryOperator::NotEq => "!=",
                    BinaryOperator::Less => "<",
                    BinaryOperator::LessEq => "<=",
                    BinaryOperator::Greater => ">",
                    BinaryOperator::GreaterEq => ">=",
                    BinaryOperator::Add => "+",
                    BinaryOperator::Subtract => "-",
                    BinaryOperator::Multiply => "*",
                    BinaryOperator::Divide => "/",
                    BinaryOperator::Modulo => "%",
                };
                format!("{} {op} {}", left.label(), right.label())
            }
            Self::Unary { operator, expr } => match operator {
                UnaryOperator::Not => format!("NOT {}", expr.label()),
                UnaryOperator::Negate => format!("-{}", expr.label()),
                UnaryOperator::Positive => format!("+{}", expr.label()),
            },
        }
    }

    pub(crate) fn contains_aggregate(&self) -> bool {
        match self {
            Self::Aggregate { .. } => true,
            Self::Binary { left, right, .. } => {
                left.contains_aggregate() || right.contains_aggregate()
            }
            Self::Unary { expr, .. } => expr.contains_aggregate(),
            Self::Literal(_) | Self::Column(_) => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum TokenKind {
    Word(String),
    QuotedIdentifier(String),
    String(String),
    Number(String),
    LeftParen,
    RightParen,
    Comma,
    Semicolon,
    Dot,
    Star,
    Plus,
    Minus,
    Slash,
    Percent,
    Eq,
    NotEq,
    Less,
    LessEq,
    Greater,
    GreaterEq,
    Eof,
}

#[derive(Debug, Clone, PartialEq)]
struct Token {
    kind: TokenKind,
    offset: usize,
}

pub(crate) fn parse(
    input: &str,
    max_expression_depth: usize,
    max_expression_nodes: usize,
    max_string_bytes: usize,
    max_tokens: usize,
    max_statements: usize,
) -> Result<Vec<Statement>, DatabaseError> {
    let tokens = Lexer::new(input, max_string_bytes, max_tokens, max_statements).tokenize()?;
    Parser::new(
        tokens,
        max_expression_depth.min(MAX_SAFE_EXPRESSION_DEPTH),
        max_expression_nodes.min(MAX_SAFE_EXPRESSION_NODES),
        max_statements,
    )
    .parse_statements()
}

struct Lexer<'a> {
    input: &'a str,
    offset: usize,
    max_string_bytes: usize,
    max_tokens: usize,
    max_statements: usize,
}

impl<'a> Lexer<'a> {
    fn new(
        input: &'a str,
        max_string_bytes: usize,
        max_tokens: usize,
        max_statements: usize,
    ) -> Self {
        Self {
            input,
            offset: 0,
            max_string_bytes,
            max_tokens,
            max_statements,
        }
    }

    fn tokenize(mut self) -> Result<Vec<Token>, DatabaseError> {
        let mut tokens = Vec::new();
        let mut statements = 0_usize;
        let mut at_statement_start = true;
        while self.offset < self.input.len() {
            self.skip_space_and_comments()?;
            if self.offset >= self.input.len() {
                break;
            }
            let start = self.offset;
            let ch = self.next_char().expect("offset is within input");
            let kind = match ch {
                '(' => TokenKind::LeftParen,
                ')' => TokenKind::RightParen,
                ',' => TokenKind::Comma,
                ';' => TokenKind::Semicolon,
                '.' => TokenKind::Dot,
                '*' => TokenKind::Star,
                '+' => TokenKind::Plus,
                '-' => TokenKind::Minus,
                '/' => TokenKind::Slash,
                '%' => TokenKind::Percent,
                '=' => TokenKind::Eq,
                '<' if self.consume_char('=') => TokenKind::LessEq,
                '<' if self.consume_char('>') => TokenKind::NotEq,
                '<' => TokenKind::Less,
                '>' if self.consume_char('=') => TokenKind::GreaterEq,
                '>' => TokenKind::Greater,
                '!' if self.consume_char('=') => TokenKind::NotEq,
                '!' => return Err(DatabaseError::parse("expected '=' after '!'", start)),
                '\'' => TokenKind::String(self.quoted('\'', start)?),
                '"' => TokenKind::QuotedIdentifier(self.quoted('"', start)?),
                '`' => TokenKind::QuotedIdentifier(self.quoted('`', start)?),
                ch if is_identifier_start(ch) => {
                    while self.peek_char().is_some_and(is_identifier_continue) {
                        self.next_char();
                    }
                    TokenKind::Word(self.input[start..self.offset].to_owned())
                }
                ch if ch.is_ascii_digit() => TokenKind::Number(self.number(start)?),
                _ => {
                    return Err(DatabaseError::parse(
                        format!("unexpected character {ch:?}"),
                        start,
                    ));
                }
            };
            if tokens.len() >= self.max_tokens {
                return Err(DatabaseError::LimitExceeded {
                    kind: LimitKind::RequestTokens,
                    limit: self.max_tokens,
                    actual: tokens.len() + 1,
                });
            }
            if matches!(kind, TokenKind::Semicolon) {
                at_statement_start = true;
            } else if at_statement_start {
                if statements >= self.max_statements {
                    return Err(DatabaseError::LimitExceeded {
                        kind: LimitKind::RequestStatements,
                        limit: self.max_statements,
                        actual: statements + 1,
                    });
                }
                statements += 1;
                at_statement_start = false;
            }
            tokens.push(Token {
                kind,
                offset: start,
            });
        }
        tokens.push(Token {
            kind: TokenKind::Eof,
            offset: self.input.len(),
        });
        Ok(tokens)
    }

    fn skip_space_and_comments(&mut self) -> Result<(), DatabaseError> {
        loop {
            while self.peek_char().is_some_and(char::is_whitespace) {
                self.next_char();
            }
            if self.input[self.offset..].starts_with("--") {
                while self.peek_char().is_some_and(|ch| ch != '\n') {
                    self.next_char();
                }
            } else if self.input[self.offset..].starts_with("/*") {
                let start = self.offset;
                self.offset += 2;
                if let Some(end) = self.input[self.offset..].find("*/") {
                    self.offset += end + 2;
                } else {
                    return Err(DatabaseError::parse("unterminated block comment", start));
                }
            } else {
                return Ok(());
            }
        }
    }

    fn quoted(&mut self, quote: char, start: usize) -> Result<String, DatabaseError> {
        let mut value = String::new();
        while let Some(ch) = self.next_char() {
            if ch == quote {
                if self.peek_char() == Some(quote) {
                    self.next_char();
                    value.push(quote);
                } else {
                    return Ok(value);
                }
            } else if ch == '\\' && quote == '\'' {
                let escaped = self
                    .next_char()
                    .ok_or_else(|| DatabaseError::parse("unterminated string", start))?;
                value.push(match escaped {
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    other => other,
                });
            } else {
                value.push(ch);
            }
            if quote == '\'' && value.len() > self.max_string_bytes {
                return Err(DatabaseError::LimitExceeded {
                    kind: LimitKind::StringBytes,
                    limit: self.max_string_bytes,
                    actual: value.len(),
                });
            }
        }
        Err(DatabaseError::parse("unterminated quoted value", start))
    }

    fn number(&mut self, start: usize) -> Result<String, DatabaseError> {
        while self.peek_char().is_some_and(|ch| ch.is_ascii_digit()) {
            self.next_char();
        }
        if self.peek_char() == Some('.') {
            self.next_char();
            while self.peek_char().is_some_and(|ch| ch.is_ascii_digit()) {
                self.next_char();
            }
        }
        if self.peek_char().is_some_and(|ch| matches!(ch, 'e' | 'E')) {
            self.next_char();
            if self.peek_char().is_some_and(|ch| matches!(ch, '+' | '-')) {
                self.next_char();
            }
            let exponent_start = self.offset;
            while self.peek_char().is_some_and(|ch| ch.is_ascii_digit()) {
                self.next_char();
            }
            if exponent_start == self.offset {
                return Err(DatabaseError::parse("invalid numeric exponent", start));
            }
        }
        Ok(self.input[start..self.offset].to_owned())
    }

    fn next_char(&mut self) -> Option<char> {
        let ch = self.peek_char()?;
        self.offset += ch.len_utf8();
        Some(ch)
    }

    fn peek_char(&self) -> Option<char> {
        self.input[self.offset..].chars().next()
    }

    fn consume_char(&mut self, expected: char) -> bool {
        if self.peek_char() == Some(expected) {
            self.next_char();
            true
        } else {
            false
        }
    }
}

fn is_identifier_start(ch: char) -> bool {
    ch == '_' || ch.is_alphabetic()
}

fn is_identifier_continue(ch: char) -> bool {
    ch == '_' || ch == '$' || ch.is_alphanumeric()
}

struct Parser {
    tokens: Vec<Token>,
    position: usize,
    expression_depth: usize,
    expression_nodes: usize,
    max_expression_depth: usize,
    max_expression_nodes: usize,
    max_statements: usize,
}

impl Parser {
    fn new(
        tokens: Vec<Token>,
        max_expression_depth: usize,
        max_expression_nodes: usize,
        max_statements: usize,
    ) -> Self {
        Self {
            tokens,
            position: 0,
            expression_depth: 0,
            expression_nodes: 0,
            max_expression_depth,
            max_expression_nodes,
            max_statements,
        }
    }

    fn parse_statements(mut self) -> Result<Vec<Statement>, DatabaseError> {
        let mut statements = Vec::new();
        while !self.at_eof() {
            while self.consume(&TokenKind::Semicolon) {}
            if self.at_eof() {
                break;
            }
            if statements.len() >= self.max_statements {
                return Err(DatabaseError::LimitExceeded {
                    kind: LimitKind::RequestStatements,
                    limit: self.max_statements,
                    actual: statements.len() + 1,
                });
            }
            let statement = self.parse_statement()?;
            validate_expression_depth(&statement, self.max_expression_depth)?;
            statements.push(statement);
            if !self.consume(&TokenKind::Semicolon) && !self.at_eof() {
                return Err(self.error("expected ';' between statements"));
            }
        }
        if statements.is_empty() {
            return Err(self.error("expected a SQL statement"));
        }
        Ok(statements)
    }

    fn parse_statement(&mut self) -> Result<Statement, DatabaseError> {
        if self.consume_keyword("CREATE") {
            self.parse_create()
        } else if self.consume_keyword("INSERT") {
            self.parse_insert()
        } else if self.consume_keyword("SELECT") {
            Ok(Statement::Select(self.parse_select()?))
        } else {
            Err(self.error("expected CREATE, INSERT, or SELECT"))
        }
    }

    fn parse_create(&mut self) -> Result<Statement, DatabaseError> {
        self.expect_keyword("TABLE")?;
        let if_not_exists = if self.consume_keyword("IF") {
            self.expect_keyword("NOT")?;
            self.expect_keyword("EXISTS")?;
            true
        } else {
            false
        };
        let name = self.parse_identifier()?;
        self.expect(TokenKind::LeftParen, "expected '(' after table name")?;
        let mut columns = Vec::new();
        loop {
            let name = self.parse_identifier()?;
            let type_name = self.parse_identifier()?;
            let data_type = match type_name.value.to_ascii_uppercase().as_str() {
                "INT64" | "BIGINT" => DataType::Int64,
                "FLOAT64" | "DOUBLE" => DataType::Float64,
                "BOOL" | "BOOLEAN" => DataType::Bool,
                "STRING" | "TEXT" => DataType::String,
                _ => {
                    return Err(self.error(format!("unsupported data type {}", type_name.value)));
                }
            };
            columns.push(CreateColumn { name, data_type });
            if !self.consume(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(TokenKind::RightParen, "expected ')' after columns")?;
        Ok(Statement::CreateTable {
            name,
            if_not_exists,
            columns,
        })
    }

    fn parse_insert(&mut self) -> Result<Statement, DatabaseError> {
        self.expect_keyword("INTO")?;
        let table = self.parse_identifier()?;
        let columns = if self.consume(&TokenKind::LeftParen) {
            let mut columns = Vec::new();
            loop {
                columns.push(self.parse_identifier()?);
                if !self.consume(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(TokenKind::RightParen, "expected ')' after insert columns")?;
            Some(columns)
        } else {
            None
        };
        self.expect_keyword("VALUES")?;
        let mut rows = Vec::new();
        loop {
            self.expect(TokenKind::LeftParen, "expected '(' before values row")?;
            let mut row = Vec::new();
            if !self.consume(&TokenKind::RightParen) {
                loop {
                    row.push(self.parse_expression()?);
                    if !self.consume(&TokenKind::Comma) {
                        break;
                    }
                }
                self.expect(TokenKind::RightParen, "expected ')' after values row")?;
            }
            rows.push(row);
            if !self.consume(&TokenKind::Comma) {
                break;
            }
        }
        Ok(Statement::Insert {
            table,
            columns,
            rows,
        })
    }

    fn parse_select(&mut self) -> Result<Select, DatabaseError> {
        let mut items = Vec::new();
        loop {
            if self.consume(&TokenKind::Star) {
                items.push(SelectItem::Wildcard);
            } else {
                let expr = self.parse_expression()?;
                let alias = if self.consume_keyword("AS")
                    || (self.token_is_identifier() && !self.at_select_clause())
                {
                    Some(self.parse_identifier()?)
                } else {
                    None
                };
                items.push(SelectItem::Expr { expr, alias });
            }
            if !self.consume(&TokenKind::Comma) {
                break;
            }
        }

        let from = if self.consume_keyword("FROM") {
            Some(self.parse_identifier()?)
        } else {
            None
        };
        let filter = if self.consume_keyword("WHERE") {
            Some(self.parse_expression()?)
        } else {
            None
        };
        let mut group_by = Vec::new();
        if self.consume_keyword("GROUP") {
            self.expect_keyword("BY")?;
            loop {
                group_by.push(self.parse_expression()?);
                if !self.consume(&TokenKind::Comma) {
                    break;
                }
            }
        }
        let mut order_by = Vec::new();
        if self.consume_keyword("ORDER") {
            self.expect_keyword("BY")?;
            loop {
                let expr = self.parse_expression()?;
                let descending = if self.consume_keyword("DESC") {
                    true
                } else {
                    self.consume_keyword("ASC");
                    false
                };
                order_by.push(OrderBy { expr, descending });
                if !self.consume(&TokenKind::Comma) {
                    break;
                }
            }
        }

        let mut limit = None;
        let mut offset = 0;
        if self.consume_keyword("LIMIT") {
            let first = self.parse_usize("LIMIT")?;
            if self.consume(&TokenKind::Comma) {
                offset = first;
                limit = Some(self.parse_usize("LIMIT")?);
            } else {
                limit = Some(first);
                if self.consume_keyword("OFFSET") {
                    offset = self.parse_usize("OFFSET")?;
                }
            }
        }
        Ok(Select {
            items,
            from,
            filter,
            group_by,
            order_by,
            limit,
            offset,
        })
    }

    fn parse_expression(&mut self) -> Result<Expr, DatabaseError> {
        if self.expression_depth == 0 {
            self.expression_nodes = 0;
        }
        if self.expression_depth >= self.max_expression_depth {
            return Err(DatabaseError::LimitExceeded {
                kind: LimitKind::ExpressionDepth,
                limit: self.max_expression_depth,
                actual: self.expression_depth + 1,
            });
        }
        self.expression_depth += 1;
        let result = self.parse_or();
        self.expression_depth -= 1;
        result
    }

    fn parse_or(&mut self) -> Result<Expr, DatabaseError> {
        let mut expr = self.parse_and()?;
        while self.consume_keyword("OR") {
            self.record_expression_node()?;
            expr = Expr::Binary {
                left: Box::new(expr),
                operator: BinaryOperator::Or,
                right: Box::new(self.parse_and()?),
            };
        }
        Ok(expr)
    }

    fn parse_and(&mut self) -> Result<Expr, DatabaseError> {
        let mut expr = self.parse_not()?;
        while self.consume_keyword("AND") {
            self.record_expression_node()?;
            expr = Expr::Binary {
                left: Box::new(expr),
                operator: BinaryOperator::And,
                right: Box::new(self.parse_not()?),
            };
        }
        Ok(expr)
    }

    fn parse_not(&mut self) -> Result<Expr, DatabaseError> {
        let mut count = 0_usize;
        while self.consume_keyword("NOT") {
            count += 1;
        }
        let mut expr = self.parse_comparison()?;
        for _ in 0..count {
            self.record_expression_node()?;
            expr = Expr::Unary {
                operator: UnaryOperator::Not,
                expr: Box::new(expr),
            };
        }
        Ok(expr)
    }

    fn parse_comparison(&mut self) -> Result<Expr, DatabaseError> {
        let mut expr = self.parse_additive()?;
        let operator = if self.consume(&TokenKind::Eq) {
            Some(BinaryOperator::Eq)
        } else if self.consume(&TokenKind::NotEq) {
            Some(BinaryOperator::NotEq)
        } else if self.consume(&TokenKind::Less) {
            Some(BinaryOperator::Less)
        } else if self.consume(&TokenKind::LessEq) {
            Some(BinaryOperator::LessEq)
        } else if self.consume(&TokenKind::Greater) {
            Some(BinaryOperator::Greater)
        } else if self.consume(&TokenKind::GreaterEq) {
            Some(BinaryOperator::GreaterEq)
        } else {
            None
        };
        if let Some(operator) = operator {
            self.record_expression_node()?;
            expr = Expr::Binary {
                left: Box::new(expr),
                operator,
                right: Box::new(self.parse_additive()?),
            };
        }
        Ok(expr)
    }

    fn parse_additive(&mut self) -> Result<Expr, DatabaseError> {
        let mut expr = self.parse_multiplicative()?;
        loop {
            let operator = if self.consume(&TokenKind::Plus) {
                BinaryOperator::Add
            } else if self.consume(&TokenKind::Minus) {
                BinaryOperator::Subtract
            } else {
                break;
            };
            self.record_expression_node()?;
            expr = Expr::Binary {
                left: Box::new(expr),
                operator,
                right: Box::new(self.parse_multiplicative()?),
            };
        }
        Ok(expr)
    }

    fn parse_multiplicative(&mut self) -> Result<Expr, DatabaseError> {
        let mut expr = self.parse_unary()?;
        loop {
            let operator = if self.consume(&TokenKind::Star) {
                BinaryOperator::Multiply
            } else if self.consume(&TokenKind::Slash) {
                BinaryOperator::Divide
            } else if self.consume(&TokenKind::Percent) {
                BinaryOperator::Modulo
            } else {
                break;
            };
            self.record_expression_node()?;
            expr = Expr::Binary {
                left: Box::new(expr),
                operator,
                right: Box::new(self.parse_unary()?),
            };
        }
        Ok(expr)
    }

    fn parse_unary(&mut self) -> Result<Expr, DatabaseError> {
        let mut operators = Vec::new();
        loop {
            if self.consume(&TokenKind::Minus) {
                if operators.is_empty()
                    && let TokenKind::Number(value) = self.current().kind.clone()
                    && !value.contains(['.', 'e', 'E'])
                {
                    let offset = self.current().offset.saturating_sub(1);
                    self.advance();
                    let number = format!("-{value}").parse::<i64>().map_err(|_| {
                        DatabaseError::parse(format!("invalid Int64 literal -{value}"), offset)
                    })?;
                    self.record_expression_node()?;
                    return Ok(Expr::Literal(Value::Int64(number)));
                }
                operators.push(UnaryOperator::Negate);
            } else if self.consume(&TokenKind::Plus) {
                operators.push(UnaryOperator::Positive);
            } else {
                break;
            }
        }
        let mut expr = self.parse_primary()?;
        for operator in operators.into_iter().rev() {
            self.record_expression_node()?;
            expr = Expr::Unary {
                operator,
                expr: Box::new(expr),
            };
        }
        Ok(expr)
    }

    fn parse_primary(&mut self) -> Result<Expr, DatabaseError> {
        if self.consume(&TokenKind::LeftParen) {
            let expr = self.parse_expression()?;
            self.expect(TokenKind::RightParen, "expected ')' after expression")?;
            return Ok(expr);
        }
        if let TokenKind::String(value) = self.current().kind.clone() {
            self.advance();
            self.record_expression_node()?;
            return Ok(Expr::Literal(Value::String(value)));
        }
        if let TokenKind::Number(value) = self.current().kind.clone() {
            let offset = self.current().offset;
            self.advance();
            if value.contains(['.', 'e', 'E']) {
                let number = value.parse::<f64>().map_err(|_| {
                    DatabaseError::parse(format!("invalid Float64 literal {value}"), offset)
                })?;
                if !number.is_finite() {
                    return Err(DatabaseError::parse("float must be finite", offset));
                }
                self.record_expression_node()?;
                return Ok(Expr::Literal(Value::Float64(number)));
            }
            let number = value.parse::<i64>().map_err(|_| {
                DatabaseError::parse(format!("invalid Int64 literal {value}"), offset)
            })?;
            self.record_expression_node()?;
            return Ok(Expr::Literal(Value::Int64(number)));
        }
        if self.consume_keyword("TRUE") {
            self.record_expression_node()?;
            return Ok(Expr::Literal(Value::Bool(true)));
        }
        if self.consume_keyword("FALSE") {
            self.record_expression_node()?;
            return Ok(Expr::Literal(Value::Bool(false)));
        }
        if self.peek_keyword("NULL") {
            return Err(self.error("NULL is not supported by the non-nullable type system"));
        }

        let name = self.parse_identifier()?;
        if self.consume(&TokenKind::LeftParen) {
            let function = match name.value.to_ascii_uppercase().as_str() {
                "COUNT" => AggregateFunction::Count,
                "SUM" => AggregateFunction::Sum,
                "MIN" => AggregateFunction::Min,
                "MAX" => AggregateFunction::Max,
                "AVG" => AggregateFunction::Avg,
                _ => return Err(self.error(format!("unsupported function {}", name.value))),
            };
            let argument = if self.consume(&TokenKind::Star) {
                None
            } else if self.consume(&TokenKind::RightParen) {
                if function == AggregateFunction::Count {
                    self.record_expression_node()?;
                    return Ok(Expr::Aggregate {
                        function,
                        argument: None,
                    });
                }
                return Err(self.error(format!("{} requires an argument", function.name())));
            } else {
                Some(Box::new(self.parse_expression()?))
            };
            self.expect(
                TokenKind::RightParen,
                "expected ')' after function argument",
            )?;
            if function != AggregateFunction::Count && argument.is_none() {
                return Err(self.error(format!("{}(*) is not supported", function.name())));
            }
            self.record_expression_node()?;
            Ok(Expr::Aggregate { function, argument })
        } else {
            let (qualifier, name) = if self.consume(&TokenKind::Dot) {
                (Some(name), self.parse_identifier()?)
            } else {
                (None, name)
            };
            self.record_expression_node()?;
            Ok(Expr::Column(ColumnReference { qualifier, name }))
        }
    }

    fn parse_identifier(&mut self) -> Result<Identifier, DatabaseError> {
        match self.current().kind.clone() {
            TokenKind::Word(value) => {
                self.advance();
                Ok(Identifier {
                    value,
                    quoted: false,
                })
            }
            TokenKind::QuotedIdentifier(value) => {
                self.advance();
                Ok(Identifier {
                    value,
                    quoted: true,
                })
            }
            _ => Err(self.error("expected an identifier")),
        }
    }

    fn record_expression_node(&mut self) -> Result<(), DatabaseError> {
        self.expression_nodes += 1;
        if self.expression_nodes > self.max_expression_nodes {
            Err(DatabaseError::LimitExceeded {
                kind: LimitKind::ExpressionNodes,
                limit: self.max_expression_nodes,
                actual: self.expression_nodes,
            })
        } else {
            Ok(())
        }
    }

    fn parse_usize(&mut self, context: &str) -> Result<usize, DatabaseError> {
        let TokenKind::Number(value) = self.current().kind.clone() else {
            return Err(self.error(format!("expected an integer after {context}")));
        };
        let offset = self.current().offset;
        self.advance();
        if value.contains(['.', 'e', 'E']) {
            return Err(DatabaseError::parse(
                format!("{context} must be a non-negative integer"),
                offset,
            ));
        }
        value
            .parse::<usize>()
            .map_err(|_| DatabaseError::parse(format!("{context} is too large"), offset))
    }

    fn at_select_clause(&self) -> bool {
        ["FROM", "WHERE", "GROUP", "ORDER", "LIMIT", "OFFSET"]
            .iter()
            .any(|keyword| self.peek_keyword(keyword))
    }

    fn token_is_identifier(&self) -> bool {
        matches!(
            self.current().kind,
            TokenKind::Word(_) | TokenKind::QuotedIdentifier(_)
        )
    }

    fn expect_keyword(&mut self, keyword: &str) -> Result<(), DatabaseError> {
        if self.consume_keyword(keyword) {
            Ok(())
        } else {
            Err(self.error(format!("expected {keyword}")))
        }
    }

    fn peek_keyword(&self, keyword: &str) -> bool {
        matches!(&self.current().kind, TokenKind::Word(word) if word.eq_ignore_ascii_case(keyword))
    }

    fn consume_keyword(&mut self, keyword: &str) -> bool {
        if self.peek_keyword(keyword) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, expected: TokenKind, message: &str) -> Result<(), DatabaseError> {
        if self.consume(&expected) {
            Ok(())
        } else {
            Err(self.error(message))
        }
    }

    fn consume(&mut self, expected: &TokenKind) -> bool {
        if &self.current().kind == expected {
            self.advance();
            true
        } else {
            false
        }
    }

    fn at_eof(&self) -> bool {
        matches!(self.current().kind, TokenKind::Eof)
    }

    fn advance(&mut self) {
        if self.position + 1 < self.tokens.len() {
            self.position += 1;
        }
    }

    fn current(&self) -> &Token {
        &self.tokens[self.position]
    }

    fn error(&self, message: impl Into<String>) -> DatabaseError {
        DatabaseError::parse(message, self.current().offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_comments_strings_and_multiple_statements() {
        let statements = parse(
            "-- setup\nCREATE TABLE `Odd Table` (`Group` String, n Int64);\n\
             INSERT INTO `Odd Table` VALUES ('a;''b', -2);\n\
             SELECT `Group` AS g, SUM(n) total FROM `Odd Table` \
             WHERE n >= -2 AND (`Group` = 'x' OR `Group` = 'a;''b') \
            GROUP BY `Group` ORDER BY total DESC LIMIT 3;",
            256,
            1024,
            1024 * 1024,
            65_536,
            1_024,
        )
        .unwrap();
        assert_eq!(statements.len(), 3);
        let Statement::Select(select) = &statements[2] else {
            panic!("expected select");
        };
        assert_eq!(select.items.len(), 2);
        assert_eq!(select.group_by.len(), 1);
        assert_eq!(select.limit, Some(3));
    }

    #[test]
    fn rejects_trailing_unparsed_tokens() {
        let error = parse(
            "SELECT 1 nonsense FROM t unexpected",
            256,
            1024,
            1024 * 1024,
            65_536,
            1_024,
        )
        .unwrap_err();
        assert!(error.to_string().contains("expected ';'"));
    }
}
