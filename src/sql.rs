use crate::error::{Error, Result};
use crate::storage::{ColumnDef, DataType, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Comparison {
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
}

#[derive(Debug, Clone)]
pub(crate) struct Predicate {
    pub(crate) column: ColumnRef,
    pub(crate) comparison: Comparison,
    pub(crate) value: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ColumnRef {
    pub(crate) qualifier: Option<String>,
    pub(crate) name: String,
}

#[derive(Debug, Clone)]
pub(crate) struct TableRef {
    pub(crate) name: String,
    pub(crate) alias: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JoinKind {
    Inner,
    Left,
}

#[derive(Debug, Clone)]
pub(crate) struct Join {
    pub(crate) kind: JoinKind,
    pub(crate) table: TableRef,
    pub(crate) equality: Vec<(ColumnRef, ColumnRef)>,
}

#[derive(Debug, Clone)]
pub(crate) enum SelectItem {
    Wildcard(Option<String>),
    Column {
        column: ColumnRef,
        alias: Option<String>,
    },
    Window {
        expression: WindowExpression,
        alias: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct WindowExpression {
    pub(crate) function: WindowFunction,
    pub(crate) partition_by: Vec<ColumnRef>,
    pub(crate) order_by: Vec<OrderBy>,
    pub(crate) frame: Option<WindowFrame>,
}

#[derive(Debug, Clone)]
pub(crate) enum WindowFunction {
    RowNumber,
    Rank,
    Sum(ColumnRef),
    Count(Option<ColumnRef>),
}

#[derive(Debug, Clone)]
pub(crate) struct OrderBy {
    pub(crate) column: ColumnRef,
    pub(crate) descending: bool,
    pub(crate) nulls_first: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WindowFrame {
    pub(crate) start: FrameBound,
    pub(crate) end: FrameBound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameBound {
    UnboundedPreceding,
    Preceding(usize),
    CurrentRow,
    Following(usize),
    UnboundedFollowing,
}

#[derive(Debug, Clone)]
pub(crate) enum Statement {
    Begin,
    Commit,
    Rollback,
    CreateTable {
        name: String,
        columns: Vec<ColumnDef>,
    },
    DropTable {
        name: String,
    },
    Insert {
        table: String,
        columns: Option<Vec<String>>,
        rows: Vec<Vec<Value>>,
    },
    Select {
        from: TableRef,
        projection: Vec<SelectItem>,
        joins: Vec<Join>,
        predicates: Vec<Predicate>,
        order_by: Vec<OrderBy>,
        limit: Option<usize>,
    },
}

pub(crate) fn parse(sql: &str) -> Result<Statement> {
    let Tokenized {
        tokens,
        positions,
        end_position,
    } = tokenize(sql)?;
    let mut parser = Parser {
        tokens,
        positions,
        end_position,
        position: 0,
        last_position: None,
    };
    let statement = parser.statement()?;
    if parser.consume_symbol(';') && parser.consume_symbol(';') {
        return Err(parser.error_at_last("only one trailing semicolon is allowed"));
    }
    if parser.peek().is_some() {
        return Err(parser.error("unexpected tokens after statement"));
    }
    Ok(statement)
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Word(String),
    QuotedIdentifier(String),
    String(String),
    Number(String),
    Symbol(char),
    Comparison(Comparison),
}

struct Tokenized {
    tokens: Vec<Token>,
    positions: Vec<usize>,
    end_position: usize,
}

fn frame_position(bound: FrameBound) -> i128 {
    match bound {
        FrameBound::UnboundedPreceding => i128::MIN,
        FrameBound::Preceding(offset) => -(i128::try_from(offset).unwrap_or(i128::MAX)),
        FrameBound::CurrentRow => 0,
        FrameBound::Following(offset) => i128::try_from(offset).unwrap_or(i128::MAX),
        FrameBound::UnboundedFollowing => i128::MAX,
    }
}

fn tokenize(input: &str) -> Result<Tokenized> {
    let chars: Vec<char> = input.chars().collect();
    let byte_positions = input
        .char_indices()
        .map(|(position, _)| position)
        .chain(std::iter::once(input.len()))
        .collect::<Vec<_>>();
    let mut tokens = Vec::new();
    let mut positions = Vec::new();
    let mut position = 0;
    while position < chars.len() {
        let character = chars[position];
        if character.is_whitespace() {
            position += 1;
            continue;
        }
        let token_position = byte_positions[position];
        if character == '\'' {
            let (value, next) = quoted(&chars, &byte_positions, position, '\'', true)?;
            tokens.push(Token::String(value));
            positions.push(token_position);
            position = next;
            continue;
        }
        if character == '"' {
            let (value, next) = quoted(&chars, &byte_positions, position, '"', false)?;
            tokens.push(Token::QuotedIdentifier(value));
            positions.push(token_position);
            position = next;
            continue;
        }
        if character.is_ascii_alphabetic() || character == '_' {
            let start = position;
            position += 1;
            while position < chars.len()
                && (chars[position].is_ascii_alphanumeric() || chars[position] == '_')
            {
                position += 1;
            }
            tokens.push(Token::Word(
                chars[start..position]
                    .iter()
                    .collect::<String>()
                    .to_ascii_lowercase(),
            ));
            positions.push(token_position);
            continue;
        }
        if character.is_ascii_digit()
            || (character == '-' && chars.get(position + 1).is_some_and(char::is_ascii_digit))
        {
            let start = position;
            position += 1;
            while position < chars.len() && chars[position].is_ascii_digit() {
                position += 1;
            }
            if chars.get(position) == Some(&'.') {
                position += 1;
                while position < chars.len() && chars[position].is_ascii_digit() {
                    position += 1;
                }
            }
            if matches!(chars.get(position), Some('e' | 'E')) {
                position += 1;
                if matches!(chars.get(position), Some('+' | '-')) {
                    position += 1;
                }
                let exponent_start = position;
                while position < chars.len() && chars[position].is_ascii_digit() {
                    position += 1;
                }
                if exponent_start == position {
                    return Err(Error::Parse {
                        message: "invalid numeric exponent".to_owned(),
                        position: byte_positions[position],
                    });
                }
            }
            tokens.push(Token::Number(chars[start..position].iter().collect()));
            positions.push(token_position);
            continue;
        }
        match character {
            '(' | ')' | ',' | '*' | ';' | '.' => {
                tokens.push(Token::Symbol(character));
                positions.push(token_position);
                position += 1;
            }
            '=' => {
                tokens.push(Token::Comparison(Comparison::Equal));
                positions.push(token_position);
                position += 1;
            }
            '!' if chars.get(position + 1) == Some(&'=') => {
                tokens.push(Token::Comparison(Comparison::NotEqual));
                positions.push(token_position);
                position += 2;
            }
            '<' => {
                if chars.get(position + 1) == Some(&'=') {
                    tokens.push(Token::Comparison(Comparison::LessOrEqual));
                    position += 2;
                } else if chars.get(position + 1) == Some(&'>') {
                    tokens.push(Token::Comparison(Comparison::NotEqual));
                    position += 2;
                } else {
                    tokens.push(Token::Comparison(Comparison::Less));
                    position += 1;
                }
                positions.push(token_position);
            }
            '>' => {
                if chars.get(position + 1) == Some(&'=') {
                    tokens.push(Token::Comparison(Comparison::GreaterOrEqual));
                    position += 2;
                } else {
                    tokens.push(Token::Comparison(Comparison::Greater));
                    position += 1;
                }
                positions.push(token_position);
            }
            _ => {
                return Err(Error::Parse {
                    message: format!("unexpected character {character:?}"),
                    position: token_position,
                });
            }
        }
    }
    if tokens.is_empty() {
        return Err(Error::Parse {
            message: "statement is empty".to_owned(),
            position: 0,
        });
    }
    Ok(Tokenized {
        tokens,
        positions,
        end_position: input.len(),
    })
}

fn quoted(
    chars: &[char],
    byte_positions: &[usize],
    start: usize,
    delimiter: char,
    backslash_escapes: bool,
) -> Result<(String, usize)> {
    let mut value = String::new();
    let mut position = start + 1;
    while position < chars.len() {
        if chars[position] == delimiter {
            if chars.get(position + 1) == Some(&delimiter) {
                value.push(delimiter);
                position += 2;
                continue;
            }
            return Ok((value, position + 1));
        }
        if backslash_escapes && chars[position] == '\\' {
            let escaped = chars.get(position + 1).ok_or_else(|| Error::Parse {
                message: "unterminated escape in string literal".to_owned(),
                position: byte_positions[position],
            })?;
            value.push(match escaped {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                '\\' => '\\',
                '\'' => '\'',
                other => *other,
            });
            position += 2;
        } else {
            value.push(chars[position]);
            position += 1;
        }
    }
    Err(Error::Parse {
        message: format!(
            "unterminated {}",
            if delimiter == '\'' {
                "string literal"
            } else {
                "quoted identifier"
            }
        ),
        position: byte_positions[start],
    })
}

struct Parser {
    tokens: Vec<Token>,
    positions: Vec<usize>,
    end_position: usize,
    position: usize,
    last_position: Option<usize>,
}

impl Parser {
    fn statement(&mut self) -> Result<Statement> {
        if self.consume_keyword("begin") {
            self.consume_keyword("transaction");
            return Ok(Statement::Begin);
        }
        if self.consume_keyword("start") {
            self.expect_keyword("transaction")?;
            return Ok(Statement::Begin);
        }
        if self.consume_keyword("commit") {
            self.consume_keyword("transaction");
            return Ok(Statement::Commit);
        }
        if self.consume_keyword("rollback") {
            self.consume_keyword("transaction");
            return Ok(Statement::Rollback);
        }
        if self.consume_keyword("create") {
            return self.create_table();
        }
        if self.consume_keyword("drop") {
            return self.drop_table();
        }
        if self.consume_keyword("insert") {
            return self.insert();
        }
        if self.consume_keyword("select") {
            return self.select();
        }
        Err(self.error("expected BEGIN, COMMIT, ROLLBACK, CREATE, DROP, INSERT, or SELECT"))
    }

    fn create_table(&mut self) -> Result<Statement> {
        self.expect_keyword("table")?;
        let name = self.identifier()?;
        self.expect_symbol('(')?;
        let mut columns = Vec::new();
        loop {
            let name = self.identifier()?;
            let (data_type, mut nullable) = if self.consume_keyword("nullable") {
                self.expect_symbol('(')?;
                let data_type = self.data_type()?;
                self.expect_symbol(')')?;
                (data_type, true)
            } else {
                (self.data_type()?, false)
            };
            if self.consume_keyword("not") {
                self.expect_keyword("null")?;
                nullable = false;
            } else if self.consume_keyword("null") {
                nullable = true;
            }
            columns.push(ColumnDef {
                name,
                data_type,
                nullable,
            });
            if !self.consume_symbol(',') {
                break;
            }
        }
        self.expect_symbol(')')?;
        Ok(Statement::CreateTable { name, columns })
    }

    fn drop_table(&mut self) -> Result<Statement> {
        self.expect_keyword("table")?;
        Ok(Statement::DropTable {
            name: self.identifier()?,
        })
    }

    fn insert(&mut self) -> Result<Statement> {
        self.expect_keyword("into")?;
        let table = self.identifier()?;
        let columns = if self.consume_symbol('(') {
            let mut columns = Vec::new();
            loop {
                columns.push(self.identifier()?);
                if !self.consume_symbol(',') {
                    break;
                }
            }
            self.expect_symbol(')')?;
            Some(columns)
        } else {
            None
        };
        self.expect_keyword("values")?;
        let mut rows = Vec::new();
        loop {
            self.expect_symbol('(')?;
            let mut row = Vec::new();
            loop {
                row.push(self.literal()?);
                if !self.consume_symbol(',') {
                    break;
                }
            }
            self.expect_symbol(')')?;
            rows.push(row);
            if !self.consume_symbol(',') {
                break;
            }
        }
        Ok(Statement::Insert {
            table,
            columns,
            rows,
        })
    }

    fn select(&mut self) -> Result<Statement> {
        let mut projection = Vec::new();
        loop {
            projection.push(self.select_item()?);
            if !self.consume_symbol(',') {
                break;
            }
        }
        self.expect_keyword("from")?;
        let from = self.table_ref()?;
        let mut joins = Vec::new();
        loop {
            let kind = if self.consume_keyword("inner") {
                self.expect_keyword("join")?;
                Some(JoinKind::Inner)
            } else if self.consume_keyword("left") {
                self.consume_keyword("outer");
                self.expect_keyword("join")?;
                Some(JoinKind::Left)
            } else if self.consume_keyword("join") {
                Some(JoinKind::Inner)
            } else {
                None
            };
            let Some(kind) = kind else {
                break;
            };
            let table = self.table_ref()?;
            self.expect_keyword("on")?;
            let mut equality = Vec::new();
            loop {
                let left = self.column_ref()?;
                match self.next() {
                    Some(Token::Comparison(Comparison::Equal)) => {}
                    _ => return Err(self.error_at_last("joins require an equality predicate")),
                }
                equality.push((left, self.column_ref()?));
                if !self.consume_keyword("and") {
                    break;
                }
            }
            joins.push(Join {
                kind,
                table,
                equality,
            });
        }
        let mut predicates = Vec::new();
        if self.consume_keyword("where") {
            loop {
                let column = self.column_ref()?;
                let comparison = match self.next() {
                    Some(Token::Comparison(comparison)) => comparison,
                    _ => return Err(self.error_at_last("expected a comparison operator")),
                };
                let value = self.literal()?;
                predicates.push(Predicate {
                    column,
                    comparison,
                    value,
                });
                if !self.consume_keyword("and") {
                    break;
                }
            }
        }
        let order_by = if self.consume_keyword("order") {
            self.expect_keyword("by")?;
            self.order_by_list()?
        } else {
            Vec::new()
        };
        let limit = if self.consume_keyword("limit") {
            Some(self.usize_literal("LIMIT requires a nonnegative integer")?)
        } else {
            None
        };
        Ok(Statement::Select {
            from,
            projection,
            joins,
            predicates,
            order_by,
            limit,
        })
    }

    fn select_item(&mut self) -> Result<SelectItem> {
        if self.consume_symbol('*') {
            return Ok(SelectItem::Wildcard(None));
        }
        if self.is_window_function() {
            let expression = self.window_expression()?;
            return Ok(SelectItem::Window {
                expression,
                alias: self.optional_alias()?,
            });
        }

        let first = self.identifier()?;
        if self.consume_symbol('.') {
            if self.consume_symbol('*') {
                return Ok(SelectItem::Wildcard(Some(first)));
            }
            let column = ColumnRef {
                qualifier: Some(first),
                name: self.identifier()?,
            };
            return Ok(SelectItem::Column {
                column,
                alias: self.optional_alias()?,
            });
        }
        Ok(SelectItem::Column {
            column: ColumnRef {
                qualifier: None,
                name: first,
            },
            alias: self.optional_alias()?,
        })
    }

    fn is_window_function(&self) -> bool {
        matches!(
            (self.tokens.get(self.position), self.tokens.get(self.position + 1)),
            (
                Some(Token::Word(name)),
                Some(Token::Symbol('('))
            ) if matches!(name.as_str(), "row_number" | "rank" | "sum" | "count")
        )
    }

    fn window_expression(&mut self) -> Result<WindowExpression> {
        let name = self.identifier()?;
        self.expect_symbol('(')?;
        let function = match name.as_str() {
            "row_number" => {
                self.expect_symbol(')')?;
                WindowFunction::RowNumber
            }
            "rank" => {
                self.expect_symbol(')')?;
                WindowFunction::Rank
            }
            "sum" => {
                let column = self.column_ref()?;
                self.expect_symbol(')')?;
                WindowFunction::Sum(column)
            }
            "count" => {
                let column = if self.consume_symbol('*') {
                    None
                } else {
                    Some(self.column_ref()?)
                };
                self.expect_symbol(')')?;
                WindowFunction::Count(column)
            }
            _ => return Err(self.error_at_last("unsupported window function")),
        };
        self.expect_keyword("over")?;
        self.expect_symbol('(')?;
        let partition_by = if self.consume_keyword("partition") {
            self.expect_keyword("by")?;
            self.column_ref_list()?
        } else {
            Vec::new()
        };
        let order_by = if self.consume_keyword("order") {
            self.expect_keyword("by")?;
            self.order_by_list()?
        } else {
            Vec::new()
        };
        let frame = if self.consume_keyword("rows") {
            Some(self.window_frame()?)
        } else {
            None
        };
        self.expect_symbol(')')?;
        Ok(WindowExpression {
            function,
            partition_by,
            order_by,
            frame,
        })
    }

    fn window_frame(&mut self) -> Result<WindowFrame> {
        let (start, end) = if self.consume_keyword("between") {
            let start = self.frame_bound()?;
            self.expect_keyword("and")?;
            (start, self.frame_bound()?)
        } else {
            (self.frame_bound()?, FrameBound::CurrentRow)
        };
        if start == FrameBound::UnboundedFollowing {
            return Err(self.error_at_last("a ROWS frame may not start with UNBOUNDED FOLLOWING"));
        }
        if end == FrameBound::UnboundedPreceding {
            return Err(self.error_at_last("a ROWS frame may not end with UNBOUNDED PRECEDING"));
        }
        if frame_position(start) > frame_position(end) {
            return Err(self.error_at_last("a ROWS frame start must not follow its end"));
        }
        Ok(WindowFrame { start, end })
    }

    fn frame_bound(&mut self) -> Result<FrameBound> {
        if self.consume_keyword("unbounded") {
            if self.consume_keyword("preceding") {
                return Ok(FrameBound::UnboundedPreceding);
            }
            self.expect_keyword("following")?;
            return Ok(FrameBound::UnboundedFollowing);
        }
        if self.consume_keyword("current") {
            self.expect_keyword("row")?;
            return Ok(FrameBound::CurrentRow);
        }
        let offset = self.usize_literal("a ROWS frame offset must be a nonnegative integer")?;
        if self.consume_keyword("preceding") {
            Ok(FrameBound::Preceding(offset))
        } else if self.consume_keyword("following") {
            Ok(FrameBound::Following(offset))
        } else {
            Err(self.error("expected PRECEDING or FOLLOWING"))
        }
    }

    fn order_by_list(&mut self) -> Result<Vec<OrderBy>> {
        let mut order_by = Vec::new();
        loop {
            let column = self.column_ref()?;
            let descending = if self.consume_keyword("desc") {
                true
            } else {
                self.consume_keyword("asc");
                false
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
            order_by.push(OrderBy {
                column,
                descending,
                nulls_first,
            });
            if !self.consume_symbol(',') {
                break;
            }
        }
        Ok(order_by)
    }

    fn column_ref_list(&mut self) -> Result<Vec<ColumnRef>> {
        let mut columns = Vec::new();
        loop {
            columns.push(self.column_ref()?);
            if !self.consume_symbol(',') {
                break;
            }
        }
        Ok(columns)
    }

    fn column_ref(&mut self) -> Result<ColumnRef> {
        let first = self.identifier()?;
        if self.consume_symbol('.') {
            Ok(ColumnRef {
                qualifier: Some(first),
                name: self.identifier()?,
            })
        } else {
            Ok(ColumnRef {
                qualifier: None,
                name: first,
            })
        }
    }

    fn table_ref(&mut self) -> Result<TableRef> {
        let name = self.identifier()?;
        let alias = self.optional_alias()?;
        Ok(TableRef { name, alias })
    }

    fn optional_alias(&mut self) -> Result<Option<String>> {
        if self.consume_keyword("as") {
            return self.identifier().map(Some);
        }
        let is_clause = matches!(
            self.peek(),
            Some(Token::Word(word))
                if matches!(
                    word.as_str(),
                    "from" | "inner" | "left" | "outer" | "join" | "on" | "where"
                        | "order" | "limit" | "and" | "rows" | "partition" | "asc"
                        | "desc" | "nulls" | "first" | "last" | "over"
                )
        );
        if is_clause || matches!(self.peek(), Some(Token::Symbol(',' | ')'))) {
            Ok(None)
        } else if matches!(
            self.peek(),
            Some(Token::Word(_) | Token::QuotedIdentifier(_))
        ) {
            self.identifier().map(Some)
        } else {
            Ok(None)
        }
    }

    fn usize_literal(&mut self, message: &str) -> Result<usize> {
        match self.next() {
            Some(Token::Number(value))
                if !value.contains(['.', 'e', 'E']) && !value.starts_with('-') =>
            {
                value
                    .parse::<usize>()
                    .map_err(|_| self.error_at_last(message))
            }
            _ => Err(self.error_at_last(message)),
        }
    }

    fn data_type(&mut self) -> Result<DataType> {
        match self.next() {
            Some(Token::Word(value)) if value == "int64" || value == "bigint" => {
                Ok(DataType::Int64)
            }
            Some(Token::Word(value)) if value == "float64" || value == "double" => {
                Ok(DataType::Float64)
            }
            Some(Token::Word(value)) if value == "bool" || value == "boolean" => Ok(DataType::Bool),
            Some(Token::Word(value)) if value == "string" || value == "text" => {
                Ok(DataType::String)
            }
            _ => Err(self.error_at_last("expected Int64, Float64, Bool, or String")),
        }
    }

    fn literal(&mut self) -> Result<Value> {
        match self.next() {
            Some(Token::String(value)) => Ok(Value::String(value)),
            Some(Token::Number(value)) if value.contains(['.', 'e', 'E']) => value
                .parse::<f64>()
                .map(Value::Float64)
                .map_err(|_| self.error_at_last("invalid Float64 literal")),
            Some(Token::Number(value)) => value
                .parse::<i64>()
                .map(Value::Int64)
                .map_err(|_| self.error_at_last("invalid Int64 literal")),
            Some(Token::Word(value)) if value == "true" => Ok(Value::Bool(true)),
            Some(Token::Word(value)) if value == "false" => Ok(Value::Bool(false)),
            Some(Token::Word(value)) if value == "null" => Ok(Value::Null),
            _ => Err(self.error_at_last("expected a literal value")),
        }
    }

    fn identifier(&mut self) -> Result<String> {
        match self.next() {
            Some(Token::Word(value) | Token::QuotedIdentifier(value)) => Ok(value),
            _ => Err(self.error_at_last("expected an identifier")),
        }
    }

    fn expect_keyword(&mut self, expected: &str) -> Result<()> {
        if self.consume_keyword(expected) {
            Ok(())
        } else {
            Err(self.error(&format!("expected {expected}")))
        }
    }

    fn consume_keyword(&mut self, expected: &str) -> bool {
        if matches!(self.peek(), Some(Token::Word(value)) if value == expected) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect_symbol(&mut self, expected: char) -> Result<()> {
        if self.consume_symbol(expected) {
            Ok(())
        } else {
            Err(self.error(&format!("expected {expected}")))
        }
    }

    fn consume_symbol(&mut self, expected: char) -> bool {
        if self.peek() == Some(&Token::Symbol(expected)) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.position).cloned();
        if token.is_some() {
            self.advance();
        } else {
            self.last_position = Some(self.end_position);
        }
        token
    }

    fn advance(&mut self) {
        self.last_position = self.positions.get(self.position).copied();
        self.position += 1;
    }

    fn error(&self, message: &str) -> Error {
        let position = self
            .positions
            .get(self.position)
            .copied()
            .unwrap_or(self.end_position);
        Self::parse_error(message, position)
    }

    fn error_at_last(&self, message: &str) -> Error {
        Self::parse_error(message, self.last_position.unwrap_or(self.end_position))
    }

    fn parse_error(message: &str, position: usize) -> Error {
        Error::Parse {
            message: message.to_owned(),
            position,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_transaction_and_data_statements() {
        assert!(matches!(
            parse("BEGIN TRANSACTION;").unwrap(),
            Statement::Begin
        ));
        let insert = parse("INSERT INTO t (id, name) VALUES (1, 'a'), (-2, 'b''s')").unwrap();
        match insert {
            Statement::Insert { rows, .. } => {
                assert_eq!(rows[0], vec![Value::Int64(1), Value::String("a".into())]);
                assert_eq!(rows[1][1], Value::String("b's".into()));
            }
            _ => panic!("expected insert"),
        }
    }

    #[test]
    fn quoted_identifiers_are_not_keywords() {
        for sql in [
            "\"begin\"",
            "\"commit\"",
            "\"rollback\"",
            "\"start\" \"transaction\"",
            "\"create\" \"table\" t (id Int64)",
        ] {
            assert!(matches!(parse(sql), Err(Error::Parse { .. })), "{sql:?}");
        }

        let Statement::CreateTable { name, columns } =
            parse("CREATE TABLE \"create\" (\"select\" Int64)").unwrap()
        else {
            panic!("expected CREATE TABLE");
        };
        assert_eq!(name, "create");
        assert_eq!(columns[0].name, "select");
    }

    #[test]
    fn rejects_trailing_input() {
        assert!(parse("BEGIN nope").is_err());
        assert!(parse("SELECT * FROM t;;").is_err());
    }

    #[test]
    fn reports_statement_parse_errors_at_byte_offsets() {
        for (sql, expected) in [
            ("SELECT * nope", "nope"),
            ("SELECT * FROM t WHERE id nope 1", "nope"),
            ("SELECT * FROM \"é\" @", "@"),
        ] {
            let Error::Parse { position, .. } = parse(sql).unwrap_err() else {
                panic!("expected a parse error for {sql:?}");
            };
            assert_eq!(position, sql.find(expected).unwrap(), "{sql:?}");
        }

        let sql = "SELECT * FROM";
        let Error::Parse { position, .. } = parse(sql).unwrap_err() else {
            panic!("expected a parse error");
        };
        assert_eq!(position, sql.len());
    }
}
