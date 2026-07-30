//! Typed, schema-resolved logical plans for SELECT statements.

use std::fmt;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::sql::{
    AggregateArgument, AggregateFunction, ComparisonOperator, Operand, OrderBy, Predicate, Select,
    SelectItem,
};
use crate::storage::Table;
use crate::value::{DataType, Value, ValueRef};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultColumn {
    pub name: String,
    pub data_type: DataType,
}

/// A SELECT resolved against one table schema.
#[derive(Debug, Clone, PartialEq)]
pub struct LogicalPlan {
    pub root: PlanNode,
    pub output_columns: Vec<ResultColumn>,
    node_count: usize,
}

impl LogicalPlan {
    pub fn build(table: &Table, select: &Select) -> Result<Self> {
        let predicate = select
            .predicate
            .as_ref()
            .map(|predicate| resolve_predicate(table, predicate))
            .transpose()?;
        let group_by = resolve_group_columns(table, &select.group_by)?;
        let (projection, output_columns, aggregates) =
            resolve_select_items(table, &select.items, &group_by)?;
        let ordering = resolve_ordering(&output_columns, &select.order_by)?;
        let grouped = !group_by.is_empty() || !aggregates.is_empty();

        let mut builder = PlanBuilder::default();
        let source_columns = table
            .schema()
            .iter()
            .enumerate()
            .map(|(index, column)| ResolvedColumn {
                index,
                name: column.name.clone(),
                data_type: column.data_type,
            })
            .collect();
        let mut root = builder.node(LogicalOperator::Scan {
            table: table.name().to_owned(),
            columns: source_columns,
        });
        if let Some(predicate) = predicate {
            root = builder.node(LogicalOperator::Filter {
                input: Box::new(root),
                predicate,
            });
        }
        if grouped {
            root = builder.node(LogicalOperator::Aggregation {
                input: Box::new(root),
                group_by,
                aggregates,
            });
        }
        root = builder.node(LogicalOperator::Projection {
            input: Box::new(root),
            columns: projection,
        });
        root = match (ordering.is_empty(), select.limit) {
            (false, Some(limit)) => builder.node(LogicalOperator::TopK {
                input: Box::new(root),
                ordering,
                limit,
            }),
            (false, None) => builder.node(LogicalOperator::Sort {
                input: Box::new(root),
                ordering,
            }),
            (true, Some(limit)) => builder.node(LogicalOperator::Limit {
                input: Box::new(root),
                limit,
            }),
            (true, None) => root,
        };

        Ok(Self {
            root,
            output_columns,
            node_count: builder.next_id,
        })
    }

    #[must_use]
    pub(crate) fn node_count(&self) -> usize {
        self.node_count
    }

    #[must_use]
    pub fn explain(&self) -> Vec<String> {
        self.explain_with_metrics(None)
    }

    #[must_use]
    pub(crate) fn explain_analyze(&self, metrics: &[OperatorMetrics]) -> Vec<String> {
        assert_eq!(metrics.len(), self.node_count);
        self.explain_with_metrics(Some(metrics))
    }

    fn explain_with_metrics(&self, metrics: Option<&[OperatorMetrics]>) -> Vec<String> {
        let mut lines = Vec::with_capacity(self.node_count);
        format_node(&self.root, 0, metrics, &mut lines);
        lines
    }
}

/// One node in a logical plan tree.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanNode {
    id: usize,
    pub operator: LogicalOperator,
}

impl PlanNode {
    #[must_use]
    pub(crate) fn id(&self) -> usize {
        self.id
    }
}

/// Operators produced by SELECT planning and consumed by the executor.
#[derive(Debug, Clone, PartialEq)]
pub enum LogicalOperator {
    Scan {
        table: String,
        columns: Vec<ResolvedColumn>,
    },
    Filter {
        input: Box<PlanNode>,
        predicate: PredicateExpression,
    },
    Aggregation {
        input: Box<PlanNode>,
        group_by: Vec<ResolvedColumn>,
        aggregates: Vec<AggregateExpression>,
    },
    Projection {
        input: Box<PlanNode>,
        columns: Vec<ProjectedColumn>,
    },
    Sort {
        input: Box<PlanNode>,
        ordering: Vec<SortExpression>,
    },
    TopK {
        input: Box<PlanNode>,
        ordering: Vec<SortExpression>,
        limit: usize,
    },
    Limit {
        input: Box<PlanNode>,
        limit: usize,
    },
}

impl LogicalOperator {
    pub(crate) fn input(&self) -> Option<&PlanNode> {
        match self {
            Self::Scan { .. } => None,
            Self::Filter { input, .. }
            | Self::Aggregation { input, .. }
            | Self::Projection { input, .. }
            | Self::Sort { input, .. }
            | Self::TopK { input, .. }
            | Self::Limit { input, .. } => Some(input),
        }
    }

    fn summary(&self) -> String {
        match self {
            Self::Scan { table, columns } => format!(
                "Scan [table={table}, columns=[{}]]",
                columns
                    .iter()
                    .map(|column| format!("{}:{}", column.name, column.data_type))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Filter { predicate, .. } => format!("Filter [predicate={predicate}]"),
            Self::Aggregation {
                group_by,
                aggregates,
                ..
            } => format!(
                "Aggregation [group_by=[{}], aggregates=[{}]]",
                group_by
                    .iter()
                    .map(ResolvedColumn::reference)
                    .collect::<Vec<_>>()
                    .join(", "),
                aggregates
                    .iter()
                    .map(AggregateExpression::description)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Projection { columns, .. } => format!(
                "Projection [{}]",
                columns
                    .iter()
                    .map(|column| format!("{}={}", column.output.name, column.expression))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Sort { ordering, .. } => {
                format!("Sort [order_by=[{}]]", format_ordering(ordering))
            }
            Self::TopK {
                ordering, limit, ..
            } => format!(
                "TopK [order_by=[{}], limit={limit}]",
                format_ordering(ordering)
            ),
            Self::Limit { limit, .. } => format!("Limit [limit={limit}]"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedColumn {
    pub index: usize,
    pub name: String,
    pub data_type: DataType,
}

impl ResolvedColumn {
    fn reference(&self) -> String {
        format!("{}#{}", self.name, self.index)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectedColumn {
    pub expression: ProjectionExpression,
    pub output: ResultColumn,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProjectionExpression {
    Column {
        source: ResolvedColumn,
        group_position: Option<usize>,
    },
    Aggregate {
        state: usize,
        expression: String,
    },
}

impl fmt::Display for ProjectionExpression {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Column { source, .. } => formatter.write_str(&source.reference()),
            Self::Aggregate { expression, .. } => formatter.write_str(expression),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AggregateExpression {
    pub function: AggregateFunction,
    pub argument: Option<ResolvedColumn>,
    pub output_type: DataType,
}

impl AggregateExpression {
    #[must_use]
    pub fn description(&self) -> String {
        let argument = self
            .argument
            .as_ref()
            .map_or_else(|| "*".to_owned(), ResolvedColumn::reference);
        format!("{}({argument})", self.function.name())
    }

    #[must_use]
    pub fn input_type(&self) -> Option<DataType> {
        self.argument.as_ref().map(|argument| argument.data_type)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortExpression {
    pub output: usize,
    pub name: String,
    pub descending: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PredicateExpression {
    Comparison {
        left: ResolvedOperand,
        operator: ComparisonOperator,
        right: ResolvedOperand,
    },
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
}

impl PredicateExpression {
    pub(crate) fn evaluate(&self, table: &Table, row: usize) -> bool {
        match self {
            Self::Comparison {
                left,
                operator,
                right,
            } => {
                let comparison = left
                    .value(table, row)
                    .sql_cmp(right.value(table, row))
                    .expect("predicate operand types are resolved");
                match operator {
                    ComparisonOperator::Equal => comparison.is_eq(),
                    ComparisonOperator::NotEqual => !comparison.is_eq(),
                    ComparisonOperator::Less => comparison.is_lt(),
                    ComparisonOperator::LessOrEqual => !comparison.is_gt(),
                    ComparisonOperator::Greater => comparison.is_gt(),
                    ComparisonOperator::GreaterOrEqual => !comparison.is_lt(),
                }
            }
            Self::And(left, right) => left.evaluate(table, row) && right.evaluate(table, row),
            Self::Or(left, right) => left.evaluate(table, row) || right.evaluate(table, row),
        }
    }
}

impl fmt::Display for PredicateExpression {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Comparison {
                left,
                operator,
                right,
            } => write!(formatter, "{left} {} {right}", comparison_symbol(*operator)),
            Self::And(left, right) => write!(formatter, "({left} AND {right})"),
            Self::Or(left, right) => write!(formatter, "({left} OR {right})"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedOperand {
    Column(ResolvedColumn),
    Literal(Value),
}

impl ResolvedOperand {
    fn data_type(&self) -> DataType {
        match self {
            Self::Column(column) => column.data_type,
            Self::Literal(value) => value.data_type(),
        }
    }

    fn value<'a>(&'a self, table: &'a Table, row: usize) -> ValueRef<'a> {
        match self {
            Self::Column(column) => table.columns()[column.index].value_ref(row),
            Self::Literal(value) => value.as_ref(),
        }
    }
}

impl fmt::Display for ResolvedOperand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Column(column) => formatter.write_str(&column.reference()),
            Self::Literal(Value::String(value)) => {
                write!(formatter, "'{}'", value.replace('\'', "''"))
            }
            Self::Literal(value) => formatter.write_str(&value.as_display_string()),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct OperatorMetrics {
    pub rows: usize,
    pub elapsed: Duration,
}

#[derive(Default)]
struct PlanBuilder {
    next_id: usize,
}

impl PlanBuilder {
    fn node(&mut self, operator: LogicalOperator) -> PlanNode {
        let node = PlanNode {
            id: self.next_id,
            operator,
        };
        self.next_id += 1;
        node
    }
}

fn resolve_group_columns(table: &Table, names: &[String]) -> Result<Vec<ResolvedColumn>> {
    let mut columns = Vec::with_capacity(names.len());
    for name in names {
        let index = table.column_index(name)?;
        if columns
            .iter()
            .any(|column: &ResolvedColumn| column.index == index)
        {
            return Err(Error::InvalidQuery(format!(
                "GROUP BY column '{name}' is listed more than once"
            )));
        }
        columns.push(resolved_column(table, index));
    }
    Ok(columns)
}

fn resolve_select_items(
    table: &Table,
    requested: &[SelectItem],
    group_by: &[ResolvedColumn],
) -> Result<(
    Vec<ProjectedColumn>,
    Vec<ResultColumn>,
    Vec<AggregateExpression>,
)> {
    let has_aggregate = requested
        .iter()
        .any(|item| matches!(item, SelectItem::Aggregate { .. }));
    if has_aggregate
        && requested
            .iter()
            .any(|item| matches!(item, SelectItem::Wildcard))
    {
        return Err(Error::InvalidQuery(
            "'*' projection cannot be combined with aggregates".to_owned(),
        ));
    }

    let mut projection = Vec::new();
    let mut output_columns = Vec::new();
    let mut aggregates = Vec::new();
    for requested_item in requested {
        match requested_item {
            SelectItem::Wildcard => {
                for source in 0..table.schema().len() {
                    let source = resolved_column(table, source);
                    let group_position = group_by
                        .iter()
                        .position(|column| column.index == source.index);
                    if !group_by.is_empty() && group_position.is_none() {
                        return Err(Error::InvalidQuery(format!(
                            "column '{}' must appear in GROUP BY",
                            source.name
                        )));
                    }
                    push_projection_column(
                        &mut projection,
                        &mut output_columns,
                        ProjectionExpression::Column {
                            source: source.clone(),
                            group_position,
                        },
                        ResultColumn {
                            name: source.name.clone(),
                            data_type: source.data_type,
                        },
                    );
                }
            }
            SelectItem::Column { name, alias } => {
                let source = resolved_column(table, table.column_index(name)?);
                let group_position = group_by
                    .iter()
                    .position(|column| column.index == source.index);
                if (has_aggregate || !group_by.is_empty()) && group_position.is_none() {
                    return Err(Error::InvalidQuery(format!(
                        "column '{name}' must appear in GROUP BY"
                    )));
                }
                let output = ResultColumn {
                    name: alias.clone().unwrap_or_else(|| source.name.clone()),
                    data_type: source.data_type,
                };
                push_projection_column(
                    &mut projection,
                    &mut output_columns,
                    ProjectionExpression::Column {
                        source,
                        group_position,
                    },
                    output,
                );
            }
            SelectItem::Aggregate {
                function,
                argument,
                alias,
            } => {
                let (argument, argument_name) = match argument {
                    AggregateArgument::Wildcard => {
                        if *function != AggregateFunction::Count {
                            return Err(Error::InvalidQuery(format!(
                                "{}(*) is not supported; use a column argument",
                                function.name()
                            )));
                        }
                        (None, "*".to_owned())
                    }
                    AggregateArgument::Column(name) => {
                        let column = resolved_column(table, table.column_index(name)?);
                        let argument_name = column.name.clone();
                        (Some(column), argument_name)
                    }
                };
                let input_type = argument.as_ref().map(|column| column.data_type);
                validate_aggregate(*function, input_type)?;
                let aggregate = AggregateExpression {
                    function: *function,
                    argument,
                    output_type: aggregate_output_type(*function, input_type),
                };
                let expression = aggregate.description();
                let state = aggregates.len();
                let output = ResultColumn {
                    name: alias
                        .clone()
                        .unwrap_or_else(|| format!("{}({argument_name})", function.name())),
                    data_type: aggregate.output_type,
                };
                aggregates.push(aggregate);
                push_projection_column(
                    &mut projection,
                    &mut output_columns,
                    ProjectionExpression::Aggregate { state, expression },
                    output,
                );
            }
        }
    }

    Ok((projection, output_columns, aggregates))
}

fn push_projection_column(
    projection: &mut Vec<ProjectedColumn>,
    output_columns: &mut Vec<ResultColumn>,
    expression: ProjectionExpression,
    output: ResultColumn,
) {
    output_columns.push(output.clone());
    projection.push(ProjectedColumn { expression, output });
}

fn validate_aggregate(function: AggregateFunction, input_type: Option<DataType>) -> Result<()> {
    if matches!(function, AggregateFunction::Sum | AggregateFunction::Avg)
        && !matches!(input_type, Some(DataType::Int64 | DataType::Float64))
    {
        return Err(Error::TypeMismatch {
            context: format!("{} argument", function.name()),
            expected: "Int64 or Float64".to_owned(),
            actual: input_type.map_or_else(|| "*".to_owned(), |value| value.to_string()),
        });
    }
    Ok(())
}

fn aggregate_output_type(function: AggregateFunction, input_type: Option<DataType>) -> DataType {
    match function {
        AggregateFunction::Count => DataType::Int64,
        AggregateFunction::Avg => DataType::Float64,
        AggregateFunction::Sum | AggregateFunction::Min | AggregateFunction::Max => {
            input_type.expect("validated aggregate column argument")
        }
    }
}

fn resolve_ordering(
    columns: &[ResultColumn],
    requested: &[OrderBy],
) -> Result<Vec<SortExpression>> {
    requested
        .iter()
        .map(|order| {
            let matches = columns
                .iter()
                .enumerate()
                .filter(|(_, column)| column.name.eq_ignore_ascii_case(&order.name))
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            match matches.as_slice() {
                [output] => Ok(SortExpression {
                    output: *output,
                    name: columns[*output].name.clone(),
                    descending: order.descending,
                }),
                [] => Err(Error::InvalidQuery(format!(
                    "ORDER BY column or alias '{}' is not in the SELECT output",
                    order.name
                ))),
                _ => Err(Error::InvalidQuery(format!(
                    "ORDER BY name '{}' is ambiguous",
                    order.name
                ))),
            }
        })
        .collect()
}

fn resolve_predicate(table: &Table, predicate: &Predicate) -> Result<PredicateExpression> {
    match predicate {
        Predicate::Comparison {
            left,
            operator,
            right,
        } => {
            let left = resolve_operand(table, left)?;
            let right = resolve_operand(table, right)?;
            if !comparable(left.data_type(), right.data_type()) {
                return Err(Error::TypeMismatch {
                    context: "WHERE comparison".to_owned(),
                    expected: left.data_type().to_string(),
                    actual: right.data_type().to_string(),
                });
            }
            Ok(PredicateExpression::Comparison {
                left,
                operator: *operator,
                right,
            })
        }
        Predicate::And(left, right) => Ok(PredicateExpression::And(
            Box::new(resolve_predicate(table, left)?),
            Box::new(resolve_predicate(table, right)?),
        )),
        Predicate::Or(left, right) => Ok(PredicateExpression::Or(
            Box::new(resolve_predicate(table, left)?),
            Box::new(resolve_predicate(table, right)?),
        )),
    }
}

fn resolve_operand(table: &Table, operand: &Operand) -> Result<ResolvedOperand> {
    match operand {
        Operand::Column(name) => Ok(ResolvedOperand::Column(resolved_column(
            table,
            table.column_index(name)?,
        ))),
        Operand::Literal(value) => Ok(ResolvedOperand::Literal(value.clone())),
    }
}

fn resolved_column(table: &Table, index: usize) -> ResolvedColumn {
    ResolvedColumn {
        index,
        name: table.schema()[index].name.clone(),
        data_type: table.schema()[index].data_type,
    }
}

fn comparable(left: DataType, right: DataType) -> bool {
    left == right
        || matches!(
            (left, right),
            (DataType::Int64, DataType::Float64) | (DataType::Float64, DataType::Int64)
        )
}

fn format_ordering(ordering: &[SortExpression]) -> String {
    ordering
        .iter()
        .map(|order| {
            format!(
                "{} {}",
                order.name,
                if order.descending { "DESC" } else { "ASC" }
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn comparison_symbol(operator: ComparisonOperator) -> &'static str {
    match operator {
        ComparisonOperator::Equal => "=",
        ComparisonOperator::NotEqual => "!=",
        ComparisonOperator::Less => "<",
        ComparisonOperator::LessOrEqual => "<=",
        ComparisonOperator::Greater => ">",
        ComparisonOperator::GreaterOrEqual => ">=",
    }
}

fn format_node(
    node: &PlanNode,
    depth: usize,
    metrics: Option<&[OperatorMetrics]>,
    lines: &mut Vec<String>,
) {
    let mut line = format!("{}{}", "  ".repeat(depth), node.operator.summary());
    if let Some(metrics) = metrics {
        let metric = &metrics[node.id];
        line.push_str(&format!(
            " [rows={}, elapsed={}ns]",
            metric.rows,
            metric.elapsed.as_nanos()
        ));
    }
    lines.push(line);
    if let Some(input) = node.operator.input() {
        format_node(input, depth + 1, metrics, lines);
    }
}
