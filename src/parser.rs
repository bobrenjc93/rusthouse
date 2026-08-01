use crate::ast::{BinaryOp, Expr, OrderItem, Select, SelectItem, Statement, UnaryOp};
use crate::error::{Error, Result};
use crate::identifier::{Identifier, ObjectName};
use crate::lexer::{Token, TokenKind, lex};
use crate::storage::ColumnSchema;
use crate::value::{DataType, Value};

pub(crate) fn parse(input: &str) -> Result<Vec<Statement>> {
    let tokens = lex(input)?;
    Parser {
        tokens,
        index: 0,
        expression_depth: 0,
        expression_nodes: 0,
    }
    .parse_script()
}

struct Parser {
    tokens: Vec<Token>,
    index: usize,
    expression_depth: usize,
    expression_nodes: usize,
}

const MAX_EXPRESSION_DEPTH: usize = 256;
const MAX_EXPRESSION_NODES: usize = 1_024;
const MAX_DATA_TYPE_DEPTH: usize = 64;

impl Parser {
    fn parse_script(&mut self) -> Result<Vec<Statement>> {
        let mut statements = Vec::new();
        while !self.at(&TokenKind::Eof) {
            while self.consume(&TokenKind::Semicolon) {}
            if self.at(&TokenKind::Eof) {
                break;
            }
            let statement = if self.consume_word("create") {
                self.parse_create()?
            } else if self.consume_word("insert") {
                self.parse_insert()?
            } else if self.consume_word("select") {
                Statement::Select(self.parse_select()?)
            } else {
                return Err(self.expected("CREATE, INSERT, or SELECT"));
            };
            statements.push(statement);
            if !self.consume(&TokenKind::Semicolon) && !self.at(&TokenKind::Eof) {
                return Err(self.expected("';' between SQL statements"));
            }
        }
        if statements.is_empty() {
            return Err(Error::parse(0, "expected a SQL statement"));
        }
        Ok(statements)
    }

    fn parse_create(&mut self) -> Result<Statement> {
        self.expect_word("table")?;
        let if_not_exists = if self.consume_word("if") {
            self.expect_word("not")?;
            self.expect_word("exists")?;
            true
        } else {
            false
        };
        let name = self.parse_object_name()?;
        self.expect(TokenKind::LeftParen, "'('")?;
        let mut columns = Vec::new();
        loop {
            let name = self.parse_identifier()?;
            let (data_type, mut nullable) = self.parse_data_type()?;
            if self.consume_word("not") {
                self.expect_word("null")?;
                nullable = false;
            } else if self.consume_word("null") {
                nullable = true;
            }
            columns.push(ColumnSchema {
                name: name.value,
                data_type,
                nullable,
                quoted: name.quoted,
            });
            if self.consume(&TokenKind::Comma) {
                continue;
            }
            self.expect(TokenKind::RightParen, "')'")?;
            break;
        }

        if self.consume_word("engine") {
            self.consume(&TokenKind::Equal);
            let engine = self.parse_word_identifier()?;
            if !engine.eq_ignore_ascii_case("memory") {
                return Err(Error::parse(
                    self.previous_position(),
                    format!("unsupported table engine '{engine}'"),
                ));
            }
            if self.consume(&TokenKind::LeftParen) {
                self.expect(TokenKind::RightParen, "')'")?;
            }
        }
        Ok(Statement::CreateTable {
            name,
            columns,
            if_not_exists,
        })
    }

    fn parse_data_type(&mut self) -> Result<(DataType, bool)> {
        self.parse_data_type_inner(0)
    }

    fn parse_data_type_inner(&mut self, depth: usize) -> Result<(DataType, bool)> {
        if depth >= MAX_DATA_TYPE_DEPTH {
            return Err(Error::Limit {
                resource: "SQL data type nesting",
                limit: MAX_DATA_TYPE_DEPTH,
            });
        }
        let name = self.parse_word_identifier()?;
        if name.eq_ignore_ascii_case("nullable") {
            self.expect(TokenKind::LeftParen, "'('")?;
            let (kind, nested_nullable) = self.parse_data_type_inner(depth + 1)?;
            if nested_nullable {
                return Err(self.expected("a non-nested Nullable type"));
            }
            self.expect(TokenKind::RightParen, "')'")?;
            return Ok((kind, true));
        }
        let kind = if name.eq_ignore_ascii_case("int64") {
            DataType::Int64
        } else if name.eq_ignore_ascii_case("float64") {
            DataType::Float64
        } else if name.eq_ignore_ascii_case("bool") || name.eq_ignore_ascii_case("boolean") {
            DataType::Bool
        } else if name.eq_ignore_ascii_case("string") {
            DataType::String
        } else {
            return Err(Error::parse(
                self.previous_position(),
                format!("unsupported data type '{name}'"),
            ));
        };
        Ok((kind, false))
    }

    fn parse_insert(&mut self) -> Result<Statement> {
        self.expect_word("into")?;
        let table = self.parse_object_name()?;
        let columns = if self.consume(&TokenKind::LeftParen) {
            let names = self.parse_identifier_list()?;
            self.expect(TokenKind::RightParen, "')'")?;
            Some(names)
        } else {
            None
        };
        self.expect_word("values")?;
        let mut rows = Vec::new();
        loop {
            self.expect(TokenKind::LeftParen, "'('")?;
            let mut row = Vec::new();
            if !self.at(&TokenKind::RightParen) {
                loop {
                    row.push(self.parse_insert_value()?);
                    if !self.consume(&TokenKind::Comma) {
                        break;
                    }
                }
            }
            self.expect(TokenKind::RightParen, "')'")?;
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

    fn parse_insert_value(&mut self) -> Result<Value> {
        let negative = self.consume(&TokenKind::Minus);
        let positive = if negative {
            false
        } else {
            self.consume(&TokenKind::Plus)
        };
        let token = self.current().clone();
        let value = match token.kind {
            TokenKind::Number(text) => {
                self.index += 1;
                parse_number(&text, token.position, negative)?
            }
            TokenKind::String(value) if !negative && !positive => {
                self.index += 1;
                Value::String(value)
            }
            TokenKind::Word(word)
                if !negative && !positive && word.eq_ignore_ascii_case("null") =>
            {
                self.index += 1;
                Value::Null
            }
            TokenKind::Word(word)
                if !negative && !positive && word.eq_ignore_ascii_case("true") =>
            {
                self.index += 1;
                Value::Bool(true)
            }
            TokenKind::Word(word)
                if !negative && !positive && word.eq_ignore_ascii_case("false") =>
            {
                self.index += 1;
                Value::Bool(false)
            }
            _ => return Err(self.expected("a literal INSERT value")),
        };
        Ok(value)
    }

    fn parse_select(&mut self) -> Result<Select> {
        let distinct = self.consume_word("distinct");
        let mut projection = Vec::new();
        loop {
            let expr = self.parse_expr(0)?;
            let alias = if self.consume_word("as") || self.current_is_alias() {
                Some(self.parse_identifier()?)
            } else {
                None
            };
            projection.push(SelectItem { expr, alias });
            if !self.consume(&TokenKind::Comma) {
                break;
            }
        }

        let table = if self.consume_word("from") {
            Some(self.parse_object_name()?)
        } else {
            None
        };
        let selection = if self.consume_word("where") {
            Some(self.parse_expr(0)?)
        } else {
            None
        };
        let group_by = if self.consume_word("group") {
            self.expect_word("by")?;
            self.parse_expr_list()?
        } else {
            Vec::new()
        };
        let having = if self.consume_word("having") {
            Some(self.parse_expr(0)?)
        } else {
            None
        };
        let order_by = if self.consume_word("order") {
            self.expect_word("by")?;
            let mut items = Vec::new();
            loop {
                let expr = self.parse_expr(0)?;
                let descending = if self.consume_word("desc") {
                    true
                } else {
                    self.consume_word("asc");
                    false
                };
                items.push(OrderItem { expr, descending });
                if !self.consume(&TokenKind::Comma) {
                    break;
                }
            }
            items
        } else {
            Vec::new()
        };

        let (limit, offset) = if self.consume_word("limit") {
            let first = self.parse_usize("LIMIT")?;
            if self.consume(&TokenKind::Comma) {
                let count = self.parse_usize("LIMIT")?;
                (Some(count), first)
            } else if self.consume_word("offset") {
                let offset = self.parse_usize("OFFSET")?;
                (Some(first), offset)
            } else {
                (Some(first), 0)
            }
        } else {
            (None, 0)
        };
        if self.consume_word("format") {
            let format = self.parse_word_identifier()?;
            if !format.eq_ignore_ascii_case("csv") && !format.eq_ignore_ascii_case("csvwithnames") {
                return Err(Error::parse(
                    self.previous_position(),
                    format!("unsupported output format '{format}'"),
                ));
            }
        }
        Ok(Select {
            distinct,
            projection,
            table,
            selection,
            group_by,
            having,
            order_by,
            limit,
            offset,
        })
    }

    fn parse_expr_list(&mut self) -> Result<Vec<Expr>> {
        let mut expressions = Vec::new();
        loop {
            expressions.push(self.parse_expr(0)?);
            if !self.consume(&TokenKind::Comma) {
                break;
            }
        }
        Ok(expressions)
    }

    fn parse_expr(&mut self, minimum_precedence: u8) -> Result<Expr> {
        if self.expression_depth == 0 {
            self.expression_nodes = 0;
        }
        if self.expression_depth >= MAX_EXPRESSION_DEPTH {
            return Err(Error::Limit {
                resource: "SQL expression nesting",
                limit: MAX_EXPRESSION_DEPTH,
            });
        }
        self.expression_depth += 1;
        let result = self.parse_expr_inner(minimum_precedence);
        self.expression_depth -= 1;
        result
    }

    fn parse_expr_inner(&mut self, minimum_precedence: u8) -> Result<Expr> {
        let mut left = self.parse_prefix()?;
        loop {
            if self.consume_word("is") {
                if 3 < minimum_precedence {
                    self.index -= 1;
                    break;
                }
                let negated = self.consume_word("not");
                self.expect_word("null")?;
                left = Expr::IsNull {
                    expr: Box::new(left),
                    negated,
                };
                self.note_expression_node()?;
                continue;
            }
            let Some((operator, precedence)) = self.current_binary_operator() else {
                break;
            };
            if precedence < minimum_precedence {
                break;
            }
            self.index += 1;
            let right = self.parse_expr(precedence + 1)?;
            left = Expr::Binary {
                left: Box::new(left),
                op: operator,
                right: Box::new(right),
            };
            self.note_expression_node()?;
        }
        Ok(left)
    }

    fn parse_prefix(&mut self) -> Result<Expr> {
        if self.consume(&TokenKind::Plus) {
            let expression = Expr::Unary {
                op: UnaryOp::Plus,
                expr: Box::new(self.parse_expr(6)?),
            };
            self.note_expression_node()?;
            return Ok(expression);
        }
        if self.consume(&TokenKind::Minus) {
            if let TokenKind::Number(text) = self.current().kind.clone() {
                let position = self.current().position;
                self.index += 1;
                self.note_expression_node()?;
                return Ok(Expr::Literal(parse_number(&text, position, true)?));
            }
            let expression = Expr::Unary {
                op: UnaryOp::Minus,
                expr: Box::new(self.parse_expr(6)?),
            };
            self.note_expression_node()?;
            return Ok(expression);
        }
        if self.consume_word("not") {
            let expression = Expr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(self.parse_expr(3)?),
            };
            self.note_expression_node()?;
            return Ok(expression);
        }
        if self.consume(&TokenKind::LeftParen) {
            let expression = self.parse_expr(0)?;
            self.expect(TokenKind::RightParen, "')'")?;
            return Ok(expression);
        }
        if self.consume(&TokenKind::Star) {
            self.note_expression_node()?;
            return Ok(Expr::Wildcard);
        }

        let token = self.current().clone();
        match token.kind {
            TokenKind::Number(text) => {
                self.index += 1;
                self.note_expression_node()?;
                Ok(Expr::Literal(parse_number(&text, token.position, false)?))
            }
            TokenKind::String(value) => {
                self.index += 1;
                self.note_expression_node()?;
                Ok(Expr::Literal(Value::String(value)))
            }
            TokenKind::Word(word) if word.eq_ignore_ascii_case("null") => {
                self.index += 1;
                self.note_expression_node()?;
                Ok(Expr::Literal(Value::Null))
            }
            TokenKind::Word(word) if word.eq_ignore_ascii_case("true") => {
                self.index += 1;
                self.note_expression_node()?;
                Ok(Expr::Literal(Value::Bool(true)))
            }
            TokenKind::Word(word) if word.eq_ignore_ascii_case("false") => {
                self.index += 1;
                self.note_expression_node()?;
                Ok(Expr::Literal(Value::Bool(false)))
            }
            TokenKind::Word(word) => {
                self.index += 1;
                if self.consume(&TokenKind::LeftParen) {
                    let mut args = Vec::new();
                    if !self.consume(&TokenKind::RightParen) {
                        loop {
                            args.push(self.parse_expr(0)?);
                            if !self.consume(&TokenKind::Comma) {
                                break;
                            }
                        }
                        self.expect(TokenKind::RightParen, "')'")?;
                    }
                    self.note_expression_node()?;
                    Ok(Expr::Function { name: word, args })
                } else {
                    let mut parts = vec![Identifier::unquoted(word)];
                    while self.consume(&TokenKind::Dot) {
                        parts.push(self.parse_identifier()?);
                    }
                    self.note_expression_node()?;
                    Ok(Expr::Column(parts))
                }
            }
            TokenKind::QuotedWord(word) => {
                self.index += 1;
                let mut parts = vec![Identifier::quoted(word)];
                while self.consume(&TokenKind::Dot) {
                    parts.push(self.parse_identifier()?);
                }
                self.note_expression_node()?;
                Ok(Expr::Column(parts))
            }
            _ => Err(self.expected("an expression")),
        }
    }

    fn current_binary_operator(&self) -> Option<(BinaryOp, u8)> {
        let pair = match &self.current().kind {
            TokenKind::Word(word) if word.eq_ignore_ascii_case("or") => (BinaryOp::Or, 1),
            TokenKind::Word(word) if word.eq_ignore_ascii_case("and") => (BinaryOp::And, 2),
            TokenKind::Equal => (BinaryOp::Equal, 3),
            TokenKind::NotEqual => (BinaryOp::NotEqual, 3),
            TokenKind::Less => (BinaryOp::Less, 3),
            TokenKind::LessEqual => (BinaryOp::LessEqual, 3),
            TokenKind::Greater => (BinaryOp::Greater, 3),
            TokenKind::GreaterEqual => (BinaryOp::GreaterEqual, 3),
            TokenKind::Plus => (BinaryOp::Add, 4),
            TokenKind::Minus => (BinaryOp::Subtract, 4),
            TokenKind::Star => (BinaryOp::Multiply, 5),
            TokenKind::Slash => (BinaryOp::Divide, 5),
            TokenKind::Percent => (BinaryOp::Modulo, 5),
            _ => return None,
        };
        Some(pair)
    }

    fn parse_identifier_list(&mut self) -> Result<Vec<Identifier>> {
        let mut names = Vec::new();
        loop {
            names.push(self.parse_identifier()?);
            if !self.consume(&TokenKind::Comma) {
                break;
            }
        }
        Ok(names)
    }

    fn parse_object_name(&mut self) -> Result<ObjectName> {
        let mut parts = vec![self.parse_identifier()?];
        while self.consume(&TokenKind::Dot) {
            parts.push(self.parse_identifier()?);
        }
        Ok(ObjectName(parts))
    }

    fn parse_identifier(&mut self) -> Result<Identifier> {
        match self.current().kind.clone() {
            TokenKind::Word(name) => {
                self.index += 1;
                Ok(Identifier::unquoted(name))
            }
            TokenKind::QuotedWord(name) => {
                self.index += 1;
                Ok(Identifier::quoted(name))
            }
            _ => Err(self.expected("an identifier")),
        }
    }

    fn parse_word_identifier(&mut self) -> Result<String> {
        match self.current().kind.clone() {
            TokenKind::Word(name) => {
                self.index += 1;
                Ok(name)
            }
            _ => Err(self.expected("an unquoted identifier")),
        }
    }

    fn parse_usize(&mut self, context: &str) -> Result<usize> {
        match self.current().kind.clone() {
            TokenKind::Number(number) if !number.contains(['.', 'e', 'E']) => {
                let position = self.current().position;
                self.index += 1;
                number.parse().map_err(|_| {
                    Error::parse(
                        position,
                        format!("{context} is too large for this platform"),
                    )
                })
            }
            _ => Err(self.expected(&format!("a non-negative integer after {context}"))),
        }
    }

    fn current_is_alias(&self) -> bool {
        match &self.current().kind {
            TokenKind::Word(word) => !matches!(
                word.to_ascii_lowercase().as_str(),
                "from"
                    | "where"
                    | "group"
                    | "having"
                    | "order"
                    | "limit"
                    | "format"
                    | "asc"
                    | "desc"
                    | "and"
                    | "or"
                    | "is"
            ),
            TokenKind::QuotedWord(_) => true,
            _ => false,
        }
    }

    fn note_expression_node(&mut self) -> Result<()> {
        self.expression_nodes += 1;
        if self.expression_nodes > MAX_EXPRESSION_NODES {
            return Err(Error::Limit {
                resource: "SQL expression nodes",
                limit: MAX_EXPRESSION_NODES,
            });
        }
        Ok(())
    }

    fn expect_word(&mut self, expected: &str) -> Result<()> {
        if self.consume_word(expected) {
            Ok(())
        } else {
            Err(self.expected(&format!("'{expected}'")))
        }
    }

    fn consume_word(&mut self, expected: &str) -> bool {
        if matches!(&self.current().kind, TokenKind::Word(word) if word.eq_ignore_ascii_case(expected))
        {
            self.index += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, expected: TokenKind, description: &str) -> Result<()> {
        if self.consume(&expected) {
            Ok(())
        } else {
            Err(self.expected(description))
        }
    }

    fn consume(&mut self, expected: &TokenKind) -> bool {
        if self.at(expected) {
            self.index += 1;
            true
        } else {
            false
        }
    }

    fn at(&self, expected: &TokenKind) -> bool {
        std::mem::discriminant(&self.current().kind) == std::mem::discriminant(expected)
    }

    fn current(&self) -> &Token {
        &self.tokens[self.index]
    }

    fn previous_position(&self) -> usize {
        self.tokens[self.index.saturating_sub(1)].position
    }

    fn expected(&self, expected: &str) -> Error {
        Error::parse(
            self.current().position,
            format!(
                "expected {expected}, found {}",
                describe(&self.current().kind)
            ),
        )
    }
}

fn parse_number(text: &str, position: usize, negative: bool) -> Result<Value> {
    let signed = if negative {
        format!("-{text}")
    } else {
        text.to_owned()
    };
    if text.contains(['.', 'e', 'E']) {
        let value = signed
            .parse::<f64>()
            .map_err(|_| Error::parse(position, format!("invalid Float64 literal '{signed}'")))?;
        if !value.is_finite() {
            return Err(Error::parse(position, "Float64 literals must be finite"));
        }
        Ok(Value::Float64(value))
    } else {
        signed
            .parse::<i64>()
            .map(Value::Int64)
            .map_err(|_| Error::parse(position, format!("invalid Int64 literal '{signed}'")))
    }
}

fn describe(kind: &TokenKind) -> String {
    match kind {
        TokenKind::Word(word) | TokenKind::QuotedWord(word) | TokenKind::Number(word) => {
            format!("'{word}'")
        }
        TokenKind::String(_) => "a string literal".to_owned(),
        TokenKind::Eof => "end of input".to_owned(),
        other => format!("'{other:?}'"),
    }
}
