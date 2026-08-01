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
    pub(crate) column: String,
    pub(crate) comparison: Comparison,
    pub(crate) value: Value,
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
        table: String,
        columns: Option<Vec<String>>,
        predicates: Vec<Predicate>,
    },
}

pub(crate) fn parse(sql: &str) -> Result<Statement> {
    let tokens = tokenize(sql)?;
    let mut parser = Parser {
        tokens,
        position: 0,
    };
    let statement = parser.statement()?;
    if parser.consume_symbol(';') && parser.consume_symbol(';') {
        return Err(parser.error("only one trailing semicolon is allowed"));
    }
    if parser.peek().is_some() {
        return Err(parser.error("unexpected tokens after statement"));
    }
    Ok(statement)
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Word(String),
    String(String),
    Number(String),
    Symbol(char),
    Comparison(Comparison),
}

fn tokenize(input: &str) -> Result<Vec<Token>> {
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut position = 0;
    while position < chars.len() {
        let character = chars[position];
        if character.is_whitespace() {
            position += 1;
            continue;
        }
        if character == '\'' {
            let (value, next) = quoted(&chars, position, '\'', true)?;
            tokens.push(Token::String(value));
            position = next;
            continue;
        }
        if character == '"' {
            let (value, next) = quoted(&chars, position, '"', false)?;
            tokens.push(Token::Word(value));
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
                        position,
                    });
                }
            }
            tokens.push(Token::Number(chars[start..position].iter().collect()));
            continue;
        }
        match character {
            '(' | ')' | ',' | '*' | ';' => {
                tokens.push(Token::Symbol(character));
                position += 1;
            }
            '=' => {
                tokens.push(Token::Comparison(Comparison::Equal));
                position += 1;
            }
            '!' if chars.get(position + 1) == Some(&'=') => {
                tokens.push(Token::Comparison(Comparison::NotEqual));
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
            }
            '>' => {
                if chars.get(position + 1) == Some(&'=') {
                    tokens.push(Token::Comparison(Comparison::GreaterOrEqual));
                    position += 2;
                } else {
                    tokens.push(Token::Comparison(Comparison::Greater));
                    position += 1;
                }
            }
            _ => {
                return Err(Error::Parse {
                    message: format!(
                        "unexpected character {character:?} at character {}",
                        position + 1
                    ),
                    position,
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
    Ok(tokens)
}

fn quoted(
    chars: &[char],
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
                position,
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
        position: start,
    })
}

struct Parser {
    tokens: Vec<Token>,
    position: usize,
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
        let columns = if self.consume_symbol('*') {
            None
        } else {
            let mut columns = Vec::new();
            loop {
                columns.push(self.identifier()?);
                if !self.consume_symbol(',') {
                    break;
                }
            }
            Some(columns)
        };
        self.expect_keyword("from")?;
        let table = self.identifier()?;
        let mut predicates = Vec::new();
        if self.consume_keyword("where") {
            loop {
                let column = self.identifier()?;
                let comparison = match self.next() {
                    Some(Token::Comparison(comparison)) => comparison,
                    _ => return Err(self.error("expected a comparison operator")),
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
        Ok(Statement::Select {
            table,
            columns,
            predicates,
        })
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
            _ => Err(self.error("expected Int64, Float64, Bool, or String")),
        }
    }

    fn literal(&mut self) -> Result<Value> {
        match self.next() {
            Some(Token::String(value)) => Ok(Value::String(value)),
            Some(Token::Number(value)) if value.contains(['.', 'e', 'E']) => value
                .parse::<f64>()
                .map(Value::Float64)
                .map_err(|_| self.error("invalid Float64 literal")),
            Some(Token::Number(value)) => value
                .parse::<i64>()
                .map(Value::Int64)
                .map_err(|_| self.error("invalid Int64 literal")),
            Some(Token::Word(value)) if value == "true" => Ok(Value::Bool(true)),
            Some(Token::Word(value)) if value == "false" => Ok(Value::Bool(false)),
            Some(Token::Word(value)) if value == "null" => Ok(Value::Null),
            _ => Err(self.error("expected a literal value")),
        }
    }

    fn identifier(&mut self) -> Result<String> {
        match self.next() {
            Some(Token::Word(value)) => Ok(value),
            _ => Err(self.error("expected an identifier")),
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
            self.position += 1;
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
            self.position += 1;
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
        self.position += usize::from(token.is_some());
        token
    }

    fn error(&self, message: &str) -> Error {
        Error::Parse {
            message: format!("{message} at token {}", self.position.saturating_add(1)),
            position: 0,
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
    fn rejects_trailing_input() {
        assert!(parse("BEGIN nope").is_err());
        assert!(parse("SELECT * FROM t;;").is_err());
    }
}
