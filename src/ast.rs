use crate::identifier::{Identifier, ObjectName};
use crate::storage::ColumnSchema;
use crate::value::Value;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Statement {
    CreateTable {
        name: ObjectName,
        columns: Vec<ColumnSchema>,
        if_not_exists: bool,
    },
    Insert {
        table: ObjectName,
        columns: Option<Vec<Identifier>>,
        rows: Vec<Vec<Value>>,
    },
    Select(Select),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Select {
    pub distinct: bool,
    pub projection: Vec<SelectItem>,
    pub table: Option<ObjectName>,
    pub selection: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    pub order_by: Vec<OrderItem>,
    pub limit: Option<usize>,
    pub offset: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SelectItem {
    pub expr: Expr,
    pub alias: Option<Identifier>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OrderItem {
    pub expr: Expr,
    pub descending: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Expr {
    Literal(Value),
    Column(Vec<Identifier>),
    Wildcard,
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    Binary {
        left: Box<Expr>,
        op: BinaryOp,
        right: Box<Expr>,
    },
    Function {
        name: String,
        args: Vec<Expr>,
    },
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
}

impl Expr {
    pub(crate) fn display_name(&self) -> String {
        match self {
            Self::Literal(Value::Null) => "NULL".to_owned(),
            Self::Literal(Value::Int64(value)) => value.to_string(),
            Self::Literal(Value::Float64(value)) => value.to_string(),
            Self::Literal(Value::Bool(value)) => value.to_string(),
            Self::Literal(Value::String(value)) => format!("'{value}'"),
            Self::Column(parts) => parts
                .iter()
                .map(|part| part.value.as_str())
                .collect::<Vec<_>>()
                .join("."),
            Self::Wildcard => "*".to_owned(),
            Self::Unary { op, expr } => {
                format!("{}{}", op.symbol(), expr.display_name())
            }
            Self::Binary { left, op, right } => format!(
                "{} {} {}",
                left.display_name(),
                op.symbol(),
                right.display_name()
            ),
            Self::Function { name, args } => {
                let args = args
                    .iter()
                    .map(Self::display_name)
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{name}({args})")
            }
            Self::IsNull { expr, negated } => format!(
                "{} IS {}NULL",
                expr.display_name(),
                if *negated { "NOT " } else { "" }
            ),
        }
    }

    pub(crate) fn contains_aggregate(&self) -> bool {
        match self {
            Self::Function { name, args } => {
                matches!(
                    name.to_ascii_lowercase().as_str(),
                    "count" | "sum" | "min" | "max" | "avg"
                ) || args.iter().any(Self::contains_aggregate)
            }
            Self::Unary { expr, .. } | Self::IsNull { expr, .. } => expr.contains_aggregate(),
            Self::Binary { left, right, .. } => {
                left.contains_aggregate() || right.contains_aggregate()
            }
            Self::Literal(_) | Self::Column(_) | Self::Wildcard => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UnaryOp {
    Plus,
    Minus,
    Not,
}

impl UnaryOp {
    fn symbol(self) -> &'static str {
        match self {
            Self::Plus => "+",
            Self::Minus => "-",
            Self::Not => "NOT ",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BinaryOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Modulo,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    And,
    Or,
}

impl BinaryOp {
    fn symbol(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Subtract => "-",
            Self::Multiply => "*",
            Self::Divide => "/",
            Self::Modulo => "%",
            Self::Equal => "=",
            Self::NotEqual => "!=",
            Self::Less => "<",
            Self::LessEqual => "<=",
            Self::Greater => ">",
            Self::GreaterEqual => ">=",
            Self::And => "AND",
            Self::Or => "OR",
        }
    }
}
