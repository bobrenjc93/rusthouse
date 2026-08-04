use crate::batch::error::{Error, Result};
use crate::batch::storage::{ColumnDef, is_reserved_column_name};
use crate::batch::value::{DataType, Value};

const MAX_PREDICATE_DEPTH: usize = 64;
const MAX_PREDICATE_NODES: usize = 256;

/// Maximum number of executable statements in one parsed batch.
pub const DEFAULT_MAX_BATCH_STATEMENTS: usize = 4_096;
/// Maximum `INSERT` rows stored across one parsed batch.
pub const DEFAULT_MAX_INSERT_ROWS: usize = 100_000;
/// Maximum `INSERT` scalar values stored across one parsed batch.
pub const DEFAULT_MAX_INSERT_VALUES: usize = 1_000_000;
/// Maximum items stored in variable-length schema and query lists across a batch.
pub const DEFAULT_MAX_AST_LIST_ITEMS: usize = 100_000;

/// Allocation limits applied while constructing a SQL batch AST.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchSqlLimits {
    pub max_statements: usize,
    pub max_insert_rows: usize,
    pub max_insert_values: usize,
    pub max_ast_list_items: usize,
}

impl Default for BatchSqlLimits {
    fn default() -> Self {
        Self {
            max_statements: DEFAULT_MAX_BATCH_STATEMENTS,
            max_insert_rows: DEFAULT_MAX_INSERT_ROWS,
            max_insert_values: DEFAULT_MAX_INSERT_VALUES,
            max_ast_list_items: DEFAULT_MAX_AST_LIST_ITEMS,
        }
    }
}

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
    ShowTables,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    /// Whether this is the deliberately narrow physical-column
    /// `SELECT DISTINCT` form.
    pub distinct: bool,
    pub items: Vec<SelectItem>,
    pub table: String,
    pub predicate: Option<Predicate>,
    pub group_by: Vec<String>,
    pub having: Option<Having>,
    pub order_by: Vec<OrderBy>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    Wildcard,
    Column {
        name: String,
        alias: Option<String>,
    },
    Cast {
        name: String,
        target_type: DataType,
        alias: Option<String>,
    },
    Length {
        name: String,
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
pub struct Having {
    pub alias: String,
    pub operator: ComparisonOperator,
    pub value: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderBy {
    pub name: String,
    pub descending: bool,
}

/// Parse one or more semicolon-separated SQL statements.
pub fn parse(input: &str) -> Result<Vec<Statement>> {
    parse_with_limits(input, BatchSqlLimits::default())
}

/// Parses a batch while allowing no executable statements.
///
/// This lets one-statement APIs report their own typed statement-count error
/// without changing the general batch parser's empty-input syntax error.
pub(crate) fn parse_allow_empty(input: &str) -> Result<Vec<Statement>> {
    Parser::new(input, BatchSqlLimits::default()).parse_script(true)
}

/// Parses a batch with an explicit executable-statement limit.
pub fn parse_with_statement_limit(input: &str, max_statements: usize) -> Result<Vec<Statement>> {
    parse_with_limits(
        input,
        BatchSqlLimits {
            max_statements,
            ..BatchSqlLimits::default()
        },
    )
}

/// Parses a batch lazily with explicit AST allocation limits.
pub fn parse_with_limits(input: &str, limits: BatchSqlLimits) -> Result<Vec<Statement>> {
    Parser::new(input, limits).parse_script(false)
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
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
    Invalid(Error),
    End,
}

#[derive(Clone, Copy)]
struct Lexer<'a> {
    input: &'a str,
    position: usize,
}

impl<'a> Lexer<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, position: 0 }
    }

    fn next_token(&mut self) -> Result<Token> {
        self.skip_ignored();
        let position = self.position;
        let Some(character) = self.current() else {
            return Ok(Token {
                kind: TokenKind::End,
                position,
            });
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
        Ok(Token { kind, position })
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

struct Parser<'a> {
    lexer: Lexer<'a>,
    current: Token,
    predicate_depth: usize,
    predicate_nodes: usize,
    insert_rows: usize,
    insert_values: usize,
    ast_list_items: usize,
    limits: BatchSqlLimits,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str, limits: BatchSqlLimits) -> Self {
        let mut lexer = Lexer::new(input);
        let current = Self::next_or_invalid(&mut lexer);
        Self {
            lexer,
            current,
            predicate_depth: 0,
            predicate_nodes: 0,
            insert_rows: 0,
            insert_values: 0,
            ast_list_items: 0,
            limits,
        }
    }

    fn next_or_invalid(lexer: &mut Lexer<'a>) -> Token {
        lexer.next_token().unwrap_or_else(|error| Token {
            kind: TokenKind::Invalid(error),
            position: lexer.position,
        })
    }

    fn advance(&mut self) {
        self.current = Self::next_or_invalid(&mut self.lexer);
    }

    fn take_kind(&mut self) -> TokenKind {
        let next = Self::next_or_invalid(&mut self.lexer);
        std::mem::replace(&mut self.current, next).kind
    }

    fn parse_script(mut self, allow_empty: bool) -> Result<Vec<Statement>> {
        let mut statements = Vec::new();
        while self.eat(&TokenKind::Semicolon) {}
        while !self.at(&TokenKind::End) {
            if statements.len() >= self.limits.max_statements {
                return Err(Error::StatementLimitExceeded {
                    statements: statements.len().saturating_add(1),
                    max_statements: self.limits.max_statements,
                });
            }
            statements.push(self.parse_statement()?);
            if !self.eat(&TokenKind::Semicolon) && !self.at(&TokenKind::End) {
                return self.error("expected ';' between statements");
            }
            while self.eat(&TokenKind::Semicolon) {}
        }
        if statements.is_empty() && !allow_empty {
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
        } else if self.eat_keyword("SHOW") {
            self.parse_show()
        } else {
            self.error("expected CREATE, INSERT, SELECT, or SHOW")
        }
    }

    fn parse_show(&mut self) -> Result<Statement> {
        self.expect_keyword("TABLES")?;
        if !self.at(&TokenKind::Semicolon) && !self.at(&TokenKind::End) {
            return self.error("unexpected trailing input after SHOW TABLES");
        }
        Ok(Statement::ShowTables)
    }

    fn parse_create(&mut self) -> Result<Statement> {
        self.expect_keyword("TABLE")?;
        let name = self.expect_identifier("table name")?;
        self.expect(&TokenKind::LeftParen, "'(' after table name")?;
        let mut columns = Vec::new();
        loop {
            self.reserve_ast_list_item()?;
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
            if self.insert_rows >= self.limits.max_insert_rows {
                return Err(Error::ResourceLimitExceeded {
                    resource: "INSERT rows",
                    actual: self.insert_rows.saturating_add(1),
                    max: self.limits.max_insert_rows,
                });
            }
            self.expect(&TokenKind::LeftParen, "'(' before row values")?;
            let mut row = Vec::new();
            if !self.at(&TokenKind::RightParen) {
                loop {
                    if self.insert_values >= self.limits.max_insert_values {
                        return Err(Error::ResourceLimitExceeded {
                            resource: "INSERT values",
                            actual: self.insert_values.saturating_add(1),
                            max: self.limits.max_insert_values,
                        });
                    }
                    row.push(self.parse_literal()?);
                    self.insert_values += 1;
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                }
            }
            self.expect(&TokenKind::RightParen, "')' after row values")?;
            rows.push(row);
            self.insert_rows += 1;
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        Ok(Statement::Insert { table, rows })
    }

    fn parse_select(&mut self) -> Result<Select> {
        if self.at_keyword("DISTINCT") {
            let lexer = self.lexer;
            let current = self.current.clone();
            let ast_list_items = self.ast_list_items;

            self.advance();
            match self.parse_distinct_select() {
                Ok(select) => return Ok(select),
                Err(distinct_error) => {
                    self.lexer = lexer;
                    self.current = current;
                    self.ast_list_items = ast_list_items;

                    return match self.parse_regular_select() {
                        Ok(select) => Ok(select),
                        Err(error @ Error::ResourceLimitExceeded { .. }) => Err(error),
                        Err(_) => Err(distinct_error),
                    };
                }
            }
        }

        self.parse_regular_select()
    }

    fn parse_regular_select(&mut self) -> Result<Select> {
        let mut items = Vec::new();
        loop {
            self.reserve_ast_list_item()?;
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
                self.reserve_ast_list_item()?;
                group_by.push(self.expect_identifier("GROUP BY column")?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }

        let having = if self.eat_keyword("HAVING") {
            let alias = self.expect_identifier("aggregate alias after HAVING")?;
            let operator = self.parse_comparison_operator()?;
            let value = self.parse_having_threshold()?;
            Some(Having {
                alias,
                operator,
                value,
            })
        } else {
            None
        };

        let mut order_by = Vec::new();
        if self.eat_keyword("ORDER") {
            self.expect_keyword("BY")?;
            loop {
                self.reserve_ast_list_item()?;
                let name = self.parse_order_by_name()?;
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
            distinct: false,
            items,
            table,
            predicate,
            group_by,
            having,
            order_by,
            limit,
        })
    }

    fn parse_distinct_select(&mut self) -> Result<Select> {
        const SHAPE: &str = "SELECT DISTINCT supports one or more unaliased columns followed by FROM <table> and an optional LIMIT";

        let mut items = Vec::new();
        loop {
            self.reserve_ast_list_item()?;
            let name = self.expect_identifier("column after DISTINCT")?;
            items.push(SelectItem::Column { name, alias: None });
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        if !self.eat_keyword("FROM") {
            return self.error(SHAPE);
        }
        let table = self.expect_identifier("table name after FROM")?;

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

        if !self.at(&TokenKind::Semicolon) && !self.at(&TokenKind::End) {
            return self.error(SHAPE);
        }

        Ok(Select {
            distinct: true,
            items,
            table,
            predicate: None,
            group_by: Vec::new(),
            having: None,
            order_by: Vec::new(),
            limit,
        })
    }

    fn reserve_ast_list_item(&mut self) -> Result<()> {
        if self.ast_list_items >= self.limits.max_ast_list_items {
            return Err(Error::ResourceLimitExceeded {
                resource: "SQL AST list items",
                actual: self.ast_list_items.saturating_add(1),
                max: self.limits.max_ast_list_items,
            });
        }
        self.ast_list_items += 1;
        Ok(())
    }

    fn parse_select_item(&mut self) -> Result<SelectItem> {
        if self.eat(&TokenKind::Star) {
            return Ok(SelectItem::Wildcard);
        }

        let position = self.position();
        let name = self.expect_identifier("column or aggregate function")?;
        if self.eat(&TokenKind::LeftParen) {
            if name.eq_ignore_ascii_case("CAST") {
                let name = self.expect_identifier("Int64 column in CAST")?;
                self.expect_keyword("AS")?;
                let type_position = self.position();
                let type_name = self.expect_identifier("Float64 target type in CAST")?;
                let target_type = DataType::parse(&type_name).ok_or_else(|| Error::Sql {
                    position: type_position,
                    message: format!("unknown CAST target type '{type_name}'; expected Float64"),
                })?;
                if target_type != DataType::Float64 {
                    return Err(Error::Sql {
                        position: type_position,
                        message: format!(
                            "unsupported CAST target type '{type_name}'; expected Float64"
                        ),
                    });
                }
                self.expect(&TokenKind::RightParen, "')' after CAST expression")?;
                let alias = self.parse_alias()?;
                return Ok(SelectItem::Cast {
                    name,
                    target_type,
                    alias,
                });
            }

            if name.eq_ignore_ascii_case("LENGTH") {
                let name = self.expect_identifier("String column in LENGTH")?;
                self.expect(&TokenKind::RightParen, "')' after LENGTH expression")?;
                let alias = self.parse_alias()?;
                return Ok(SelectItem::Length { name, alias });
            }

            let function = AggregateFunction::parse(&name).ok_or_else(|| Error::Sql {
                position,
                message: format!("unknown aggregate function '{name}'"),
            })?;
            let argument = if self.eat(&TokenKind::Star) {
                AggregateArgument::Wildcard
            } else {
                AggregateArgument::Column(self.expect_identifier("aggregate column")?)
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

    fn parse_order_by_name(&mut self) -> Result<String> {
        let name = self.expect_identifier("ORDER BY output column, expression, or alias")?;
        if name.eq_ignore_ascii_case("LENGTH") && self.eat(&TokenKind::LeftParen) {
            let argument = self.expect_identifier("String column in ORDER BY LENGTH")?;
            self.expect(
                &TokenKind::RightParen,
                "')' after ORDER BY LENGTH expression",
            )?;
            Ok(format!("LENGTH({argument})"))
        } else {
            Ok(name)
        }
    }

    fn parse_alias(&mut self) -> Result<Option<String>> {
        if self.eat_keyword("AS") {
            self.expect_identifier("alias").map(Some)
        } else {
            Ok(None)
        }
    }

    fn parse_having_threshold(&mut self) -> Result<i64> {
        let position = self.position();
        let sign = if self.eat(&TokenKind::Minus) {
            "-"
        } else if self.eat(&TokenKind::Plus) {
            "+"
        } else {
            ""
        };
        let number = self.take_number().ok_or_else(|| Error::Sql {
            position,
            message: "expected a signed Int64 after HAVING comparison".to_owned(),
        })?;
        let threshold = format!("{sign}{number}");

        threshold.parse::<i64>().map_err(|error| {
            let message = match error.kind() {
                std::num::IntErrorKind::PosOverflow | std::num::IntErrorKind::NegOverflow => {
                    format!("HAVING Int64 threshold '{threshold}' is outside the Int64 range")
                }
                _ => format!("invalid Int64 threshold '{threshold}' in HAVING comparison"),
            };
            Error::Sql { position, message }
        })
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
        self.advance();
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
                if value.eq_ignore_ascii_case("TRUE") || value.eq_ignore_ascii_case("FALSE") =>
            {
                self.parse_literal().map(Operand::Literal)
            }
            TokenKind::Identifier(_) => self
                .expect_identifier("column or literal")
                .map(Operand::Column),
            _ => self.error("expected column or literal"),
        }
    }

    fn parse_literal(&mut self) -> Result<Value> {
        if matches!(self.peek(), TokenKind::String(_)) {
            let TokenKind::String(value) = self.take_kind() else {
                unreachable!("matched string token")
            };
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
        if self.at_keyword(expected) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn at_keyword(&self, expected: &str) -> bool {
        matches!(self.peek(), TokenKind::Identifier(value) if value.eq_ignore_ascii_case(expected))
    }

    fn expect_identifier(&mut self, description: &str) -> Result<String> {
        if matches!(self.peek(), TokenKind::Identifier(_)) {
            let TokenKind::Identifier(value) = self.take_kind() else {
                unreachable!("matched identifier token")
            };
            Ok(value)
        } else {
            self.error(format!("expected {description}"))
        }
    }

    fn take_number(&mut self) -> Option<String> {
        if matches!(self.peek(), TokenKind::Number(_)) {
            let TokenKind::Number(value) = self.take_kind() else {
                unreachable!("matched number token")
            };
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
            self.advance();
            true
        } else {
            false
        }
    }

    fn at(&self, expected: &TokenKind) -> bool {
        self.peek() == expected
    }

    fn peek(&self) -> &TokenKind {
        &self.current.kind
    }

    fn position(&self) -> usize {
        self.current.position
    }

    fn error<T>(&self, message: impl Into<String>) -> Result<T> {
        if let TokenKind::Invalid(error) = self.peek() {
            return Err(error.clone());
        }
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
            "SELECT region, COUNT(*) AS rows, SUM(amount) AS total FROM sales \
             WHERE active = true AND amount >= 2.5 \
             GROUP BY region HAVING rows >= 2 ORDER BY total DESC LIMIT 3;",
        )
        .expect("valid SQL");

        let Statement::Select(select) = &statements[0] else {
            panic!("expected select");
        };
        assert_eq!(select.items.len(), 3);
        assert_eq!(select.group_by, ["region"]);
        assert_eq!(
            select.having,
            Some(Having {
                alias: "rows".to_owned(),
                operator: ComparisonOperator::GreaterOrEqual,
                value: 2,
            })
        );
        assert_eq!(select.order_by[0].name, "total");
        assert!(select.order_by[0].descending);
        assert_eq!(select.limit, Some(3));
    }

    #[test]
    fn parses_bounded_show_tables_with_an_optional_semicolon() {
        for sql in ["SHOW TABLES", "show tables;"] {
            assert_eq!(
                parse(sql).expect("valid SHOW TABLES"),
                [Statement::ShowTables]
            );
        }

        assert_eq!(
            parse_with_statement_limit("SHOW TABLES", 0).expect_err("statement limit applies"),
            Error::StatementLimitExceeded {
                statements: 1,
                max_statements: 0,
            }
        );
    }

    #[test]
    fn show_tables_rejects_trailing_input_with_a_typed_sql_error() {
        assert_eq!(
            parse("SHOW TABLES extra").expect_err("trailing input is not a SHOW clause"),
            Error::Sql {
                position: 12,
                message: "unexpected trailing input after SHOW TABLES".to_owned(),
            }
        );
    }

    #[test]
    fn parses_every_having_comparison_operator_and_signed_int64_boundaries() {
        let cases = [
            ("=", ComparisonOperator::Equal),
            ("!=", ComparisonOperator::NotEqual),
            ("<>", ComparisonOperator::NotEqual),
            ("<", ComparisonOperator::Less),
            ("<=", ComparisonOperator::LessOrEqual),
            (">", ComparisonOperator::Greater),
            (">=", ComparisonOperator::GreaterOrEqual),
        ];

        for (sql_operator, expected) in cases {
            let sql = format!(
                "SELECT SUM(amount) AS total FROM events HAVING total {sql_operator} +{}",
                i64::MAX
            );
            let statements = parse(&sql).expect("valid HAVING comparison");
            let Statement::Select(select) = &statements[0] else {
                panic!("expected select");
            };
            let having = select.having.as_ref().expect("HAVING is parsed");
            assert_eq!(having.operator, expected, "{sql_operator}");
            assert_eq!(having.value, i64::MAX, "{sql_operator}");
        }

        let statements = parse(&format!(
            "SELECT SUM(amount) AS total FROM events HAVING total >= {}",
            i64::MIN
        ))
        .expect("Int64 minimum is a valid HAVING threshold");
        let Statement::Select(select) = &statements[0] else {
            panic!("expected select");
        };
        assert_eq!(select.having.as_ref().unwrap().value, i64::MIN);
    }

    #[test]
    fn having_reports_typed_malformed_and_overflow_threshold_errors() {
        let malformed = "SELECT SUM(amount) AS total FROM events HAVING total = 1.5";
        assert_eq!(
            parse(malformed).expect_err("Float64 is not an Int64 threshold"),
            Error::Sql {
                position: malformed.find("1.5").unwrap(),
                message: "invalid Int64 threshold '1.5' in HAVING comparison".to_owned(),
            }
        );

        for threshold in ["+9223372036854775808", "-9223372036854775809"] {
            let sql = format!("SELECT SUM(amount) AS total FROM events HAVING total = {threshold}");
            assert_eq!(
                parse(&sql).expect_err("threshold is outside the Int64 range"),
                Error::Sql {
                    position: sql.find(threshold).unwrap(),
                    message: format!(
                        "HAVING Int64 threshold '{threshold}' is outside the Int64 range"
                    ),
                }
            );
        }

        let missing = "SELECT SUM(amount) AS total FROM events HAVING total = -many";
        assert_eq!(
            parse(missing).expect_err("a sign must be followed by digits"),
            Error::Sql {
                position: missing.find("-many").unwrap(),
                message: "expected a signed Int64 after HAVING comparison".to_owned(),
            }
        );
    }

    #[test]
    fn having_requires_an_int64_in_clause_order() {
        for sql in [
            "SELECT COUNT(*) AS n FROM events HAVING n = many",
            "SELECT COUNT(*) AS n FROM events HAVING n = 1 GROUP BY kind",
            "SELECT COUNT(*) AS n FROM events ORDER BY n HAVING n = 1",
        ] {
            assert!(parse(sql).is_err(), "{sql:?} must be rejected");
        }
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
    fn enforces_statement_limit_without_counting_semicolons_in_strings() {
        let statements = parse_with_statement_limit(
            "SELECT note FROM t WHERE note = 'one;two'; SELECT note FROM t",
            2,
        )
        .expect("two statements fit the limit");
        assert_eq!(statements.len(), 2);

        let error = parse_with_statement_limit(
            "SELECT note FROM t; SELECT note FROM t; SELECT note FROM t;",
            2,
        )
        .expect_err("third statement exceeds the limit");
        assert_eq!(
            error,
            Error::StatementLimitExceeded {
                statements: 3,
                max_statements: 2,
            }
        );
    }

    #[test]
    fn enforces_insert_row_and_value_allocation_limits_at_boundaries() {
        let limits = BatchSqlLimits {
            max_statements: 2,
            max_insert_rows: 2,
            max_insert_values: 3,
            max_ast_list_items: 10,
        };
        let statements = parse_with_limits("INSERT INTO t VALUES (true), (false)", limits)
            .expect("rows and values at or below limits succeed");
        assert_eq!(statements.len(), 1);

        let row_error = parse_with_limits("INSERT INTO t VALUES (true), (false), (true)", limits)
            .expect_err("third row exceeds the row limit");
        assert_eq!(
            row_error,
            Error::ResourceLimitExceeded {
                resource: "INSERT rows",
                actual: 3,
                max: 2,
            }
        );

        let cumulative_error = parse_with_limits(
            "INSERT INTO t VALUES (true); INSERT INTO t VALUES (false), (true)",
            limits,
        )
        .expect_err("row limit applies across INSERT statements");
        assert_eq!(cumulative_error, row_error);

        let value_error = parse_with_limits("INSERT INTO t VALUES (1, 2), (3, 4)", limits)
            .expect_err("fourth value exceeds the value limit");
        assert_eq!(
            value_error,
            Error::ResourceLimitExceeded {
                resource: "INSERT values",
                actual: 4,
                max: 3,
            }
        );
    }

    #[test]
    fn enforces_cumulative_ast_list_item_limit_at_the_boundary() {
        let sql = "CREATE TABLE t (a Int64, b Int64); \
            SELECT a, b FROM t GROUP BY a, b ORDER BY a, b";
        let exact_limits = BatchSqlLimits {
            max_ast_list_items: 8,
            ..BatchSqlLimits::default()
        };
        parse_with_limits(sql, exact_limits).expect("eight list items fit the limit");

        let error = parse_with_limits(
            sql,
            BatchSqlLimits {
                max_ast_list_items: 7,
                ..exact_limits
            },
        )
        .expect_err("eighth list item exceeds the cumulative limit");
        assert_eq!(
            error,
            Error::ResourceLimitExceeded {
                resource: "SQL AST list items",
                actual: 8,
                max: 7,
            }
        );
    }

    #[test]
    fn default_row_limit_stops_large_bool_insert_while_parsing_lazily() {
        let mut sql = String::from("INSERT INTO t VALUES ");
        for row in 0..=DEFAULT_MAX_INSERT_ROWS {
            if row != 0 {
                sql.push(',');
            }
            sql.push_str("(true)");
        }

        let error = parse(&sql).expect_err("row above default limit is rejected");
        assert_eq!(
            error,
            Error::ResourceLimitExceeded {
                resource: "INSERT rows",
                actual: DEFAULT_MAX_INSERT_ROWS + 1,
                max: DEFAULT_MAX_INSERT_ROWS,
            }
        );
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
