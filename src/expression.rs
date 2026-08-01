use std::{cmp::Ordering, collections::BTreeMap, str::FromStr};

use crate::{DataType, Error, Result, Value, value::compare_int_float};

/// Maximum nesting allowed in parsed or directly constructed expressions.
pub const MAX_EXPRESSION_DEPTH: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOperator {
    Plus,
    Minus,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOperator {
    Add,
    Subtract,
    Multiply,
    Divide,
    Modulo,
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
    And,
    Or,
}

/// Parsed SQL scalar expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Value),
    Column(String),
    QuotedColumn(String),
    Unary {
        operator: UnaryOperator,
        expression: Box<Self>,
    },
    Binary {
        left: Box<Self>,
        operator: BinaryOperator,
        right: Box<Self>,
    },
    IsNull {
        expression: Box<Self>,
        negated: bool,
    },
    Cast {
        expression: Box<Self>,
        target: DataType,
    },
    Case {
        operand: Option<Box<Self>>,
        branches: Vec<(Self, Self)>,
        else_expression: Option<Box<Self>>,
    },
    Function {
        name: String,
        arguments: Vec<Self>,
    },
}

/// Exact column bindings with case-folded lookup for unquoted identifiers.
#[derive(Debug, Clone, Default)]
pub struct EvaluationContext {
    values: BTreeMap<String, Value>,
    unquoted_names: BTreeMap<String, String>,
}

impl EvaluationContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_value(mut self, name: impl Into<String>, value: impl Into<Value>) -> Self {
        self.insert(name, value);
        self
    }

    pub fn insert(&mut self, name: impl Into<String>, value: impl Into<Value>) -> Option<Value> {
        let name = name.into();
        let folded = name.to_ascii_lowercase();
        let current_is_canonical = self
            .unquoted_names
            .get(&folded)
            .is_some_and(|current| current == &folded);
        if name == folded || !current_is_canonical {
            self.unquoted_names.insert(folded, name.clone());
        }
        self.values.insert(name, value.into())
    }

    pub fn get(&self, name: &str) -> Option<&Value> {
        self.unquoted_names
            .get(&name.to_ascii_lowercase())
            .and_then(|exact_name| self.values.get(exact_name))
    }

    pub fn get_quoted(&self, name: &str) -> Option<&Value> {
        self.values.get(name)
    }
}

/// Parses one SQL scalar expression.
pub fn parse(sql: &str) -> Result<Expr> {
    Parser::new(sql)?.parse()
}

/// Parses and evaluates one SQL scalar expression.
pub fn evaluate(sql: &str, context: &EvaluationContext) -> Result<Value> {
    parse(sql)?.evaluate(context)
}

impl Expr {
    pub fn evaluate(&self, context: &EvaluationContext) -> Result<Value> {
        validate_expression_depth(self)?;
        self.evaluate_inner(context)
    }

    fn evaluate_inner(&self, context: &EvaluationContext) -> Result<Value> {
        match self {
            Self::Literal(value) => Ok(value.clone()),
            Self::Column(name) => context
                .get(name)
                .cloned()
                .ok_or_else(|| Error::UnknownColumn(name.clone())),
            Self::QuotedColumn(name) => context
                .get_quoted(name)
                .cloned()
                .ok_or_else(|| Error::UnknownColumn(name.clone())),
            Self::Unary {
                operator,
                expression,
            } => evaluate_unary(*operator, expression.evaluate_inner(context)?),
            Self::Binary {
                left,
                operator,
                right,
            } => evaluate_binary_expression(left, *operator, right, context),
            Self::IsNull {
                expression,
                negated,
            } => Ok(Value::Bool(
                expression.evaluate_inner(context)?.is_null() != *negated,
            )),
            Self::Cast { expression, target } => {
                expression.evaluate_inner(context)?.cast_to(*target)
            }
            Self::Case {
                operand,
                branches,
                else_expression,
            } => evaluate_case(
                operand.as_deref(),
                branches,
                else_expression.as_deref(),
                context,
            ),
            Self::Function { name, arguments } => evaluate_function(name, arguments, context),
        }
    }

    #[allow(clippy::vec_box)] // Drop types cannot be moved out of Box safely.
    fn take_boxed_children(&mut self, pending: &mut Vec<Box<Self>>) {
        fn empty_expression() -> Box<Expr> {
            Box::new(Expr::Literal(Value::Null))
        }

        match self {
            Self::Literal(_) | Self::Column(_) | Self::QuotedColumn(_) => {}
            Self::Unary { expression, .. }
            | Self::IsNull { expression, .. }
            | Self::Cast { expression, .. } => {
                pending.push(std::mem::replace(expression, empty_expression()));
            }
            Self::Binary { left, right, .. } => {
                pending.push(std::mem::replace(left, empty_expression()));
                pending.push(std::mem::replace(right, empty_expression()));
            }
            Self::Case {
                operand,
                branches,
                else_expression,
            } => {
                pending.extend(operand.take());
                for (condition, result) in branches.drain(..) {
                    pending.push(Box::new(condition));
                    pending.push(Box::new(result));
                }
                pending.extend(else_expression.take());
            }
            Self::Function { arguments, .. } => {
                pending.extend(arguments.drain(..).map(Box::new));
            }
        }
    }
}

impl Drop for Expr {
    fn drop(&mut self) {
        // Remove recursive children before Rust's field drop glue runs so an
        // untrusted direct AST cannot overflow the stack during cleanup.
        let mut pending = Vec::new();
        self.take_boxed_children(&mut pending);
        while let Some(mut expression) = pending.pop() {
            expression.take_boxed_children(&mut pending);
        }
    }
}

fn validate_expression_depth(expression: &Expr) -> Result<()> {
    let mut pending = vec![(expression, 1_usize)];
    while let Some((expression, depth)) = pending.pop() {
        if depth > MAX_EXPRESSION_DEPTH {
            return Err(Error::ExpressionTooDeep {
                limit: MAX_EXPRESSION_DEPTH,
            });
        }
        let child_depth = depth + 1;
        match expression {
            Expr::Literal(_) | Expr::Column(_) | Expr::QuotedColumn(_) => {}
            Expr::Unary { expression, .. }
            | Expr::IsNull { expression, .. }
            | Expr::Cast { expression, .. } => pending.push((expression, child_depth)),
            Expr::Binary { left, right, .. } => {
                pending.push((left, child_depth));
                pending.push((right, child_depth));
            }
            Expr::Case {
                operand,
                branches,
                else_expression,
            } => {
                if let Some(operand) = operand {
                    pending.push((operand, child_depth));
                }
                for (condition, result) in branches {
                    pending.push((condition, child_depth));
                    pending.push((result, child_depth));
                }
                if let Some(else_expression) = else_expression {
                    pending.push((else_expression, child_depth));
                }
            }
            Expr::Function { arguments, .. } => {
                pending.extend(arguments.iter().map(|argument| (argument, child_depth)));
            }
        }
    }
    Ok(())
}

fn evaluate_unary(operator: UnaryOperator, value: Value) -> Result<Value> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    match (operator, value) {
        (UnaryOperator::Plus, value @ (Value::Int64(_) | Value::Float64(_))) => Ok(value),
        (UnaryOperator::Minus, Value::Int64(value)) => value
            .checked_neg()
            .map(Value::Int64)
            .ok_or_else(|| Error::Overflow {
                operation: "unary -".to_owned(),
            }),
        (UnaryOperator::Minus, Value::Float64(value)) => Ok(Value::Float64(-value)),
        (UnaryOperator::Not, Value::Bool(value)) => Ok(Value::Bool(!value)),
        (operator, value) => Err(type_error(
            unary_name(operator),
            match operator {
                UnaryOperator::Not => "Bool or NULL",
                _ => "numeric or NULL",
            },
            &value,
        )),
    }
}

fn evaluate_binary_expression(
    left: &Expr,
    operator: BinaryOperator,
    right: &Expr,
    context: &EvaluationContext,
) -> Result<Value> {
    let left = left.evaluate_inner(context)?;

    // These two cases are value-independent SQL short circuits. NULL still
    // requires evaluation of the other operand to distinguish TRUE/FALSE.
    if operator == BinaryOperator::And && left == Value::Bool(false) {
        return Ok(Value::Bool(false));
    }
    if operator == BinaryOperator::Or && left == Value::Bool(true) {
        return Ok(Value::Bool(true));
    }

    let right = right.evaluate_inner(context)?;
    match operator {
        BinaryOperator::And => logical_and(left, right),
        BinaryOperator::Or => logical_or(left, right),
        BinaryOperator::Add
        | BinaryOperator::Subtract
        | BinaryOperator::Multiply
        | BinaryOperator::Divide
        | BinaryOperator::Modulo => arithmetic(left, operator, right),
        BinaryOperator::Equal
        | BinaryOperator::NotEqual
        | BinaryOperator::Less
        | BinaryOperator::LessOrEqual
        | BinaryOperator::Greater
        | BinaryOperator::GreaterOrEqual => compare(left, operator, right),
    }
}

fn logical_and(left: Value, right: Value) -> Result<Value> {
    match (sql_truth(&left, "AND")?, sql_truth(&right, "AND")?) {
        (Some(false), _) | (_, Some(false)) => Ok(Value::Bool(false)),
        (Some(true), Some(true)) => Ok(Value::Bool(true)),
        _ => Ok(Value::Null),
    }
}

fn logical_or(left: Value, right: Value) -> Result<Value> {
    match (sql_truth(&left, "OR")?, sql_truth(&right, "OR")?) {
        (Some(true), _) | (_, Some(true)) => Ok(Value::Bool(true)),
        (Some(false), Some(false)) => Ok(Value::Bool(false)),
        _ => Ok(Value::Null),
    }
}

fn sql_truth(value: &Value, operation: &str) -> Result<Option<bool>> {
    match value {
        Value::Null => Ok(None),
        Value::Bool(value) => Ok(Some(*value)),
        value => Err(type_error(operation, "Bool or NULL", value)),
    }
}

fn arithmetic(left: Value, operator: BinaryOperator, right: Value) -> Result<Value> {
    if left.is_null() || right.is_null() {
        return Ok(Value::Null);
    }

    match (left, right) {
        (Value::Int64(left), Value::Int64(right)) => integer_arithmetic(left, operator, right),
        (Value::Int64(left), Value::Float64(right)) => {
            float_arithmetic(left as f64, operator, right)
        }
        (Value::Float64(left), Value::Int64(right)) => {
            float_arithmetic(left, operator, right as f64)
        }
        (Value::Float64(left), Value::Float64(right)) => float_arithmetic(left, operator, right),
        (left, right) => Err(Error::Type {
            operation: binary_name(operator).to_owned(),
            expected: "two numeric values or NULL".to_owned(),
            actual: format!("{} and {}", left.type_name(), right.type_name()),
        }),
    }
}

fn integer_arithmetic(left: i64, operator: BinaryOperator, right: i64) -> Result<Value> {
    let operation = binary_name(operator);
    match operator {
        BinaryOperator::Add => checked_integer(left.checked_add(right), operation),
        BinaryOperator::Subtract => checked_integer(left.checked_sub(right), operation),
        BinaryOperator::Multiply => checked_integer(left.checked_mul(right), operation),
        BinaryOperator::Divide => {
            if right == 0 {
                Err(Error::DivideByZero)
            } else {
                Ok(Value::Float64(left as f64 / right as f64))
            }
        }
        BinaryOperator::Modulo => {
            if right == 0 {
                Err(Error::DivideByZero)
            } else {
                checked_integer(left.checked_rem(right), operation)
            }
        }
        _ => unreachable!(),
    }
}

fn checked_integer(value: Option<i64>, operation: &str) -> Result<Value> {
    value.map(Value::Int64).ok_or_else(|| Error::Overflow {
        operation: operation.to_owned(),
    })
}

fn float_arithmetic(left: f64, operator: BinaryOperator, right: f64) -> Result<Value> {
    if matches!(operator, BinaryOperator::Divide | BinaryOperator::Modulo) && right == 0.0 {
        return Err(Error::DivideByZero);
    }
    let value = match operator {
        BinaryOperator::Add => left + right,
        BinaryOperator::Subtract => left - right,
        BinaryOperator::Multiply => left * right,
        BinaryOperator::Divide => left / right,
        BinaryOperator::Modulo => left % right,
        _ => unreachable!(),
    };
    Ok(Value::Float64(value))
}

fn compare(left: Value, operator: BinaryOperator, right: Value) -> Result<Value> {
    if left.is_null() || right.is_null() {
        return Ok(Value::Null);
    }

    let ordering = match (&left, &right) {
        (Value::Int64(left), Value::Int64(right)) => Some(left.cmp(right)),
        (Value::Int64(left), Value::Float64(right)) => compare_int_float(*left, *right),
        (Value::Float64(left), Value::Int64(right)) => {
            compare_int_float(*right, *left).map(Ordering::reverse)
        }
        (Value::Float64(left), Value::Float64(right)) => left.partial_cmp(right),
        (Value::Bool(left), Value::Bool(right)) => Some(left.cmp(right)),
        (Value::String(left), Value::String(right)) => Some(left.cmp(right)),
        _ => {
            return Err(Error::Type {
                operation: binary_name(operator).to_owned(),
                expected: "comparable values of the same type, or two numeric values".to_owned(),
                actual: format!("{} and {}", left.type_name(), right.type_name()),
            });
        }
    };

    // IEEE NaN is unordered: every ordered/equality comparison is false and
    // inequality is true. It remains a non-NULL value for IS NULL and COUNT.
    let result = match operator {
        BinaryOperator::Equal => ordering == Some(Ordering::Equal),
        BinaryOperator::NotEqual => ordering != Some(Ordering::Equal),
        BinaryOperator::Less => ordering == Some(Ordering::Less),
        BinaryOperator::LessOrEqual => matches!(ordering, Some(Ordering::Less | Ordering::Equal)),
        BinaryOperator::Greater => ordering == Some(Ordering::Greater),
        BinaryOperator::GreaterOrEqual => {
            matches!(ordering, Some(Ordering::Greater | Ordering::Equal))
        }
        _ => unreachable!(),
    };
    Ok(Value::Bool(result))
}

fn evaluate_case(
    operand: Option<&Expr>,
    branches: &[(Expr, Expr)],
    else_expression: Option<&Expr>,
    context: &EvaluationContext,
) -> Result<Value> {
    let operand = operand
        .map(|expression| expression.evaluate_inner(context))
        .transpose()?;
    for (condition, result) in branches {
        let matches = if let Some(operand) = &operand {
            compare(
                operand.clone(),
                BinaryOperator::Equal,
                condition.evaluate_inner(context)?,
            )? == Value::Bool(true)
        } else {
            sql_truth(&condition.evaluate_inner(context)?, "CASE WHEN")? == Some(true)
        };
        if matches {
            return result.evaluate_inner(context);
        }
    }
    else_expression
        .map(|expression| expression.evaluate_inner(context))
        .unwrap_or(Ok(Value::Null))
}

fn evaluate_function(name: &str, arguments: &[Expr], context: &EvaluationContext) -> Result<Value> {
    let normalized_name = name.to_ascii_lowercase();
    let name = normalized_name.as_str();
    match name {
        "coalesce" => {
            require_at_least(name, arguments, 1)?;
            for argument in arguments {
                let value = argument.evaluate_inner(context)?;
                if !value.is_null() {
                    return Ok(value);
                }
            }
            Ok(Value::Null)
        }
        "lower" | "upper" | "length" | "char_length" | "trim" | "ltrim" | "rtrim" => {
            require_exact(name, arguments, 1)?;
            let value = arguments[0].evaluate_inner(context)?;
            if value.is_null() {
                return Ok(Value::Null);
            }
            let Value::String(value) = value else {
                return Err(type_error(name, "String or NULL", &value));
            };
            match name {
                "lower" => Ok(Value::String(value.to_lowercase())),
                "upper" => Ok(Value::String(value.to_uppercase())),
                "length" | "char_length" => Ok(Value::Int64(value.chars().count() as i64)),
                "trim" => Ok(Value::String(value.trim().to_owned())),
                "ltrim" => Ok(Value::String(value.trim_start().to_owned())),
                "rtrim" => Ok(Value::String(value.trim_end().to_owned())),
                _ => unreachable!(),
            }
        }
        "concat" => {
            require_at_least(name, arguments, 1)?;
            let values = arguments
                .iter()
                .map(|argument| argument.evaluate_inner(context))
                .collect::<Result<Vec<_>>>()?;
            if values.iter().any(Value::is_null) {
                return Ok(Value::Null);
            }
            let mut output = String::new();
            for value in values {
                match value {
                    Value::String(value) => output.push_str(&value),
                    value => return Err(type_error(name, "String or NULL", &value)),
                }
            }
            Ok(Value::String(output))
        }
        "substring" | "substr" => substring(name, arguments, context),
        _ => Err(Error::InvalidArgument {
            function: name.to_owned(),
            message: "unknown scalar function".to_owned(),
        }),
    }
}

fn substring(name: &str, arguments: &[Expr], context: &EvaluationContext) -> Result<Value> {
    if !(2..=3).contains(&arguments.len()) {
        return Err(Error::InvalidArgument {
            function: name.to_owned(),
            message: format!("expected 2 or 3 arguments, got {}", arguments.len()),
        });
    }
    let values = arguments
        .iter()
        .map(|argument| argument.evaluate_inner(context))
        .collect::<Result<Vec<_>>>()?;
    if values.iter().any(Value::is_null) {
        return Ok(Value::Null);
    }
    let Value::String(value) = &values[0] else {
        return Err(type_error(name, "a String first argument", &values[0]));
    };
    let Value::Int64(start) = values[1] else {
        return Err(type_error(name, "an Int64 start", &values[1]));
    };
    if start < 1 {
        return Err(Error::InvalidArgument {
            function: name.to_owned(),
            message: "start must be at least 1".to_owned(),
        });
    }
    let length = if values.len() == 3 {
        let Value::Int64(length) = values[2] else {
            return Err(type_error(name, "an Int64 length", &values[2]));
        };
        if length < 0 {
            return Err(Error::InvalidArgument {
                function: name.to_owned(),
                message: "length cannot be negative".to_owned(),
            });
        }
        Some(length)
    } else {
        None
    };
    let Ok(start_index) = usize::try_from(start - 1) else {
        return Ok(Value::String(String::new()));
    };
    let characters = value.chars().skip(start_index);
    let output = match length {
        Some(length) => match usize::try_from(length) {
            Ok(length) => characters.take(length).collect(),
            Err(_) => characters.collect(),
        },
        None => characters.collect(),
    };
    Ok(Value::String(output))
}

fn require_exact(name: &str, arguments: &[Expr], expected: usize) -> Result<()> {
    if arguments.len() == expected {
        Ok(())
    } else {
        Err(Error::InvalidArgument {
            function: name.to_owned(),
            message: format!("expected {expected} arguments, got {}", arguments.len()),
        })
    }
}

fn require_at_least(name: &str, arguments: &[Expr], minimum: usize) -> Result<()> {
    if arguments.len() >= minimum {
        Ok(())
    } else {
        Err(Error::InvalidArgument {
            function: name.to_owned(),
            message: format!(
                "expected at least {minimum} arguments, got {}",
                arguments.len()
            ),
        })
    }
}

fn type_error(operation: &str, expected: &str, actual: &Value) -> Error {
    Error::Type {
        operation: operation.to_owned(),
        expected: expected.to_owned(),
        actual: actual.type_name().to_owned(),
    }
}

fn unary_name(operator: UnaryOperator) -> &'static str {
    match operator {
        UnaryOperator::Plus => "unary +",
        UnaryOperator::Minus => "unary -",
        UnaryOperator::Not => "NOT",
    }
}

fn binary_name(operator: BinaryOperator) -> &'static str {
    match operator {
        BinaryOperator::Add => "+",
        BinaryOperator::Subtract => "-",
        BinaryOperator::Multiply => "*",
        BinaryOperator::Divide => "/",
        BinaryOperator::Modulo => "%",
        BinaryOperator::Equal => "=",
        BinaryOperator::NotEqual => "<>",
        BinaryOperator::Less => "<",
        BinaryOperator::LessOrEqual => "<=",
        BinaryOperator::Greater => ">",
        BinaryOperator::GreaterOrEqual => ">=",
        BinaryOperator::And => "AND",
        BinaryOperator::Or => "OR",
    }
}

#[derive(Debug, Clone, PartialEq)]
enum TokenKind {
    Word(String),
    QuotedIdentifier(String),
    Number(String),
    String(String),
    LeftParen,
    RightParen,
    Comma,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
    End,
}

#[derive(Debug, Clone)]
struct Token {
    kind: TokenKind,
    position: usize,
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
            self.skip_whitespace();
            let position = self.position;
            let Some(character) = self.peek() else {
                tokens.push(Token {
                    kind: TokenKind::End,
                    position,
                });
                return Ok(tokens);
            };
            let kind = match character {
                '(' => self.single(TokenKind::LeftParen),
                ')' => self.single(TokenKind::RightParen),
                ',' => self.single(TokenKind::Comma),
                '+' => self.single(TokenKind::Plus),
                '-' => self.single(TokenKind::Minus),
                '*' => self.single(TokenKind::Star),
                '/' => self.single(TokenKind::Slash),
                '%' => self.single(TokenKind::Percent),
                '=' => self.single(TokenKind::Equal),
                '<' => {
                    self.bump();
                    match self.peek() {
                        Some('=') => {
                            self.bump();
                            TokenKind::LessOrEqual
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
                        TokenKind::GreaterOrEqual
                    } else {
                        TokenKind::Greater
                    }
                }
                '!' => {
                    self.bump();
                    if self.peek() == Some('=') {
                        self.bump();
                        TokenKind::NotEqual
                    } else {
                        return Err(parse_error("expected `=` after `!`", position));
                    }
                }
                '\'' => TokenKind::String(self.string_literal(position)?),
                '"' => TokenKind::QuotedIdentifier(self.quoted_identifier(position)?),
                value if value.is_ascii_digit() || value == '.' => {
                    TokenKind::Number(self.number(position)?)
                }
                value if is_identifier_start(value) => TokenKind::Word(self.word()),
                _ => {
                    return Err(parse_error(
                        format!("unexpected character `{character}`"),
                        position,
                    ));
                }
            };
            tokens.push(Token { kind, position });
        }
    }

    fn single(&mut self, token: TokenKind) -> TokenKind {
        self.bump();
        token
    }

    fn peek(&self) -> Option<char> {
        self.input[self.position..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let character = self.peek()?;
        self.position += character.len_utf8();
        Some(character)
    }

    fn skip_whitespace(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.bump();
        }
    }

    fn word(&mut self) -> String {
        let start = self.position;
        while self.peek().is_some_and(is_identifier_continue) {
            self.bump();
        }
        self.input[start..self.position].to_owned()
    }

    fn quoted_identifier(&mut self, start: usize) -> Result<String> {
        self.bump();
        let mut result = String::new();
        loop {
            match self.bump() {
                Some('"') if self.peek() == Some('"') => {
                    self.bump();
                    result.push('"');
                }
                Some('"') => return Ok(result),
                Some(character) => result.push(character),
                None => return Err(parse_error("unterminated quoted identifier", start)),
            }
        }
    }

    fn string_literal(&mut self, start: usize) -> Result<String> {
        self.bump();
        let mut result = String::new();
        loop {
            match self.bump() {
                Some('\'') if self.peek() == Some('\'') => {
                    self.bump();
                    result.push('\'');
                }
                Some('\'') => return Ok(result),
                Some(character) => result.push(character),
                None => return Err(parse_error("unterminated string literal", start)),
            }
        }
    }

    fn number(&mut self, start: usize) -> Result<String> {
        let mut has_digit = false;
        while self.peek().is_some_and(|value| value.is_ascii_digit()) {
            has_digit = true;
            self.bump();
        }
        if self.peek() == Some('.') {
            self.bump();
            while self.peek().is_some_and(|value| value.is_ascii_digit()) {
                has_digit = true;
                self.bump();
            }
        }
        if !has_digit {
            return Err(parse_error("expected a digit around decimal point", start));
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            self.bump();
            if matches!(self.peek(), Some('+' | '-')) {
                self.bump();
            }
            let exponent_start = self.position;
            while self.peek().is_some_and(|value| value.is_ascii_digit()) {
                self.bump();
            }
            if self.position == exponent_start {
                return Err(parse_error("expected digits in numeric exponent", start));
            }
        }
        Ok(self.input[start..self.position].to_owned())
    }
}

fn is_identifier_start(value: char) -> bool {
    value == '_' || value.is_alphabetic()
}

fn is_identifier_continue(value: char) -> bool {
    value == '_' || value.is_alphanumeric()
}

struct Parser {
    tokens: Vec<Token>,
    current: usize,
    nesting_depth: usize,
}

impl Parser {
    fn new(sql: &str) -> Result<Self> {
        Ok(Self {
            tokens: Lexer::new(sql).tokenize()?,
            current: 0,
            nesting_depth: 1,
        })
    }

    fn parse(mut self) -> Result<Expr> {
        let expression = self.parse_or()?;
        if !matches!(self.peek().kind, TokenKind::End) {
            return Err(parse_error(
                "unexpected token after expression",
                self.peek().position,
            ));
        }
        validate_expression_depth(&expression)?;
        Ok(expression)
    }

    fn parse_or(&mut self) -> Result<Expr> {
        let mut expression = self.parse_and()?;
        while self.consume_word("OR") {
            expression = checked_binary(expression, BinaryOperator::Or, self.parse_and()?)?;
        }
        Ok(expression)
    }

    fn parse_and(&mut self) -> Result<Expr> {
        let mut expression = self.parse_not()?;
        while self.consume_word("AND") {
            expression = checked_binary(expression, BinaryOperator::And, self.parse_not()?)?;
        }
        Ok(expression)
    }

    fn parse_not(&mut self) -> Result<Expr> {
        if self.consume_word("NOT") {
            Ok(Expr::Unary {
                operator: UnaryOperator::Not,
                expression: Box::new(self.nested(Self::parse_not)?),
            })
        } else {
            self.parse_comparison()
        }
    }

    fn parse_comparison(&mut self) -> Result<Expr> {
        let mut expression = self.parse_additive()?;
        loop {
            let operator = match self.peek().kind {
                TokenKind::Equal => Some(BinaryOperator::Equal),
                TokenKind::NotEqual => Some(BinaryOperator::NotEqual),
                TokenKind::Less => Some(BinaryOperator::Less),
                TokenKind::LessOrEqual => Some(BinaryOperator::LessOrEqual),
                TokenKind::Greater => Some(BinaryOperator::Greater),
                TokenKind::GreaterOrEqual => Some(BinaryOperator::GreaterOrEqual),
                _ => None,
            };
            if let Some(operator) = operator {
                self.advance();
                expression = checked_binary(expression, operator, self.parse_additive()?)?;
                continue;
            }
            if self.consume_word("IS") {
                let negated = self.consume_word("NOT");
                self.expect_word("NULL")?;
                expression = Expr::IsNull {
                    expression: Box::new(expression),
                    negated,
                };
                validate_expression_depth(&expression)?;
                continue;
            }
            break;
        }
        Ok(expression)
    }

    fn parse_additive(&mut self) -> Result<Expr> {
        let mut expression = self.parse_multiplicative()?;
        loop {
            let operator = match self.peek().kind {
                TokenKind::Plus => BinaryOperator::Add,
                TokenKind::Minus => BinaryOperator::Subtract,
                _ => break,
            };
            self.advance();
            expression = checked_binary(expression, operator, self.parse_multiplicative()?)?;
        }
        Ok(expression)
    }

    fn parse_multiplicative(&mut self) -> Result<Expr> {
        let mut expression = self.parse_unary()?;
        loop {
            let operator = match self.peek().kind {
                TokenKind::Star => BinaryOperator::Multiply,
                TokenKind::Slash => BinaryOperator::Divide,
                TokenKind::Percent => BinaryOperator::Modulo,
                _ => break,
            };
            self.advance();
            expression = checked_binary(expression, operator, self.parse_unary()?)?;
        }
        Ok(expression)
    }

    fn parse_unary(&mut self) -> Result<Expr> {
        let operator = match self.peek().kind {
            TokenKind::Plus => Some(UnaryOperator::Plus),
            TokenKind::Minus => Some(UnaryOperator::Minus),
            _ => None,
        };
        if let Some(operator) = operator {
            self.advance();
            if operator == UnaryOperator::Minus
                && matches!(&self.peek().kind, TokenKind::Number(value) if value == "9223372036854775808")
            {
                self.advance();
                return Ok(Expr::Literal(Value::Int64(i64::MIN)));
            }
            Ok(Expr::Unary {
                operator,
                expression: Box::new(self.nested(Self::parse_unary)?),
            })
        } else {
            self.parse_primary()
        }
    }

    fn parse_primary(&mut self) -> Result<Expr> {
        let token = self.advance().clone();
        match token.kind {
            TokenKind::Number(value) => parse_number(&value, token.position),
            TokenKind::String(value) => Ok(Expr::Literal(Value::String(value))),
            TokenKind::LeftParen => {
                let expression = self.nested(Self::parse_or)?;
                self.expect(TokenKind::RightParen, "expected `)`")?;
                Ok(expression)
            }
            TokenKind::QuotedIdentifier(name) => Ok(Expr::QuotedColumn(name)),
            TokenKind::Word(word) if word.eq_ignore_ascii_case("NULL") => {
                Ok(Expr::Literal(Value::Null))
            }
            TokenKind::Word(word) if word.eq_ignore_ascii_case("TRUE") => {
                Ok(Expr::Literal(Value::Bool(true)))
            }
            TokenKind::Word(word) if word.eq_ignore_ascii_case("FALSE") => {
                Ok(Expr::Literal(Value::Bool(false)))
            }
            TokenKind::Word(word) if word.eq_ignore_ascii_case("CAST") => self.parse_cast(),
            TokenKind::Word(word) if word.eq_ignore_ascii_case("CASE") => self.parse_case(),
            TokenKind::Word(word) => {
                if self.consume(TokenKind::LeftParen) {
                    self.parse_function(word)
                } else {
                    Ok(Expr::Column(word))
                }
            }
            TokenKind::End => Err(parse_error("expected an expression", token.position)),
            _ => Err(parse_error("expected an expression", token.position)),
        }
    }

    fn parse_cast(&mut self) -> Result<Expr> {
        self.expect(TokenKind::LeftParen, "expected `(` after CAST")?;
        let expression = self.nested(Self::parse_or)?;
        self.expect_word("AS")?;
        let token = self.advance().clone();
        let TokenKind::Word(name) = token.kind else {
            return Err(parse_error("expected a type name after AS", token.position));
        };
        let target = DataType::from_str(&name)
            .map_err(|()| parse_error(format!("unknown scalar type `{name}`"), token.position))?;
        self.expect(TokenKind::RightParen, "expected `)` after CAST")?;
        Ok(Expr::Cast {
            expression: Box::new(expression),
            target,
        })
    }

    fn parse_case(&mut self) -> Result<Expr> {
        let operand = if self.peek_word("WHEN") {
            None
        } else {
            Some(Box::new(self.nested(Self::parse_or)?))
        };
        let mut branches = Vec::new();
        while self.consume_word("WHEN") {
            let condition = self.nested(Self::parse_or)?;
            self.expect_word("THEN")?;
            let result = self.nested(Self::parse_or)?;
            branches.push((condition, result));
        }
        if branches.is_empty() {
            return Err(parse_error(
                "CASE requires at least one WHEN branch",
                self.peek().position,
            ));
        }
        let else_expression = if self.consume_word("ELSE") {
            Some(Box::new(self.nested(Self::parse_or)?))
        } else {
            None
        };
        self.expect_word("END")?;
        Ok(Expr::Case {
            operand,
            branches,
            else_expression,
        })
    }

    fn parse_function(&mut self, name: String) -> Result<Expr> {
        let mut arguments = Vec::new();
        if !self.consume(TokenKind::RightParen) {
            loop {
                arguments.push(self.nested(Self::parse_or)?);
                if self.consume(TokenKind::RightParen) {
                    break;
                }
                self.expect(TokenKind::Comma, "expected `,` between function arguments")?;
            }
        }
        Ok(Expr::Function {
            name: name.to_ascii_lowercase(),
            arguments,
        })
    }

    fn peek(&self) -> &Token {
        &self.tokens[self.current]
    }

    fn advance(&mut self) -> &Token {
        let token = &self.tokens[self.current];
        if !matches!(token.kind, TokenKind::End) {
            self.current += 1;
        }
        token
    }

    fn consume(&mut self, expected: TokenKind) -> bool {
        if self.peek().kind == expected {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, expected: TokenKind, message: &str) -> Result<()> {
        if self.consume(expected) {
            Ok(())
        } else {
            Err(parse_error(message, self.peek().position))
        }
    }

    fn peek_word(&self, expected: &str) -> bool {
        matches!(&self.peek().kind, TokenKind::Word(word) if word.eq_ignore_ascii_case(expected))
    }

    fn consume_word(&mut self, expected: &str) -> bool {
        if self.peek_word(expected) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect_word(&mut self, expected: &str) -> Result<()> {
        if self.consume_word(expected) {
            Ok(())
        } else {
            Err(parse_error(
                format!("expected keyword {expected}"),
                self.peek().position,
            ))
        }
    }

    fn nested<T>(&mut self, parse: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        if self.nesting_depth >= MAX_EXPRESSION_DEPTH {
            return Err(Error::ExpressionTooDeep {
                limit: MAX_EXPRESSION_DEPTH,
            });
        }
        self.nesting_depth += 1;
        let result = parse(self);
        self.nesting_depth -= 1;
        result
    }
}

fn binary(left: Expr, operator: BinaryOperator, right: Expr) -> Expr {
    Expr::Binary {
        left: Box::new(left),
        operator,
        right: Box::new(right),
    }
}

fn checked_binary(left: Expr, operator: BinaryOperator, right: Expr) -> Result<Expr> {
    let expression = binary(left, operator, right);
    validate_expression_depth(&expression)?;
    Ok(expression)
}

fn parse_number(value: &str, position: usize) -> Result<Expr> {
    if value.contains(['.', 'e', 'E']) {
        value
            .parse::<f64>()
            .map(Value::Float64)
            .map(Expr::Literal)
            .map_err(|_| parse_error("invalid Float64 literal", position))
    } else {
        value
            .parse::<i64>()
            .map(Value::Int64)
            .map(Expr::Literal)
            .map_err(|_| parse_error("Int64 literal is out of range", position))
    }
}

fn parse_error(message: impl Into<String>, position: usize) -> Error {
    Error::Parse {
        message: message.into(),
        position,
    }
}
