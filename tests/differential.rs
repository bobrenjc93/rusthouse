use std::cmp::Ordering;

use rusthouse::engine::{QueryResult, ResultColumn};
use rusthouse::sql::{
    self, AggregateArgument, AggregateFunction, ComparisonOperator, Operand, OrderBy, Predicate,
    Select, SelectItem, Statement,
};
use rusthouse::storage::ColumnDef;
use rusthouse::{DataType, Database, StatementResult, Value};

const CASES: u64 = 128;
const MAX_COLUMNS: usize = 6;
const MAX_ROWS: usize = 24;
const MAX_PREDICATE_DEPTH: usize = 3;

#[derive(Debug)]
struct ModelTable {
    schema: Vec<ColumnDef>,
    rows: Vec<Vec<Value>>,
}

#[derive(Debug, Clone, Copy)]
struct Generator {
    state: u64,
}

impl Generator {
    fn new(seed: u64) -> Self {
        Self {
            state: seed ^ 0x9e37_79b9_7f4a_7c15,
        }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn usize(&mut self, upper: usize) -> usize {
        (self.next() as usize) % upper
    }

    fn boolean(&mut self) -> bool {
        self.next() & 1 == 0
    }
}

#[test]
fn generated_sql_matches_independent_row_model() {
    for seed in 0..CASES {
        let mut generator = Generator::new(seed);
        let table = generate_table(&mut generator);
        let setup = vec![
            Statement::CreateTable {
                name: "generated".to_owned(),
                columns: table.schema.clone(),
            },
            Statement::Insert {
                table: "generated".to_owned(),
                rows: table.rows.clone(),
            },
        ];
        let setup_sql = sql::render_script(&setup);
        assert_eq!(
            sql::parse(&setup_sql).expect("generated setup reparses"),
            setup,
            "setup renderer round trip failed for seed {seed}"
        );

        let mut database = Database::new();
        database
            .execute(&setup_sql)
            .unwrap_or_else(|error| panic!("setup failed for seed {seed}: {error}\n{setup_sql}"));

        let queries = [
            projection_query(&table, &mut generator),
            global_aggregate_query(&table, &mut generator),
            grouped_aggregate_query(&table, &mut generator),
        ];
        for select in queries {
            let statement = Statement::Select(select.clone());
            let query_sql = sql::render(&statement);
            assert_eq!(
                sql::parse(&query_sql).expect("generated query reparses"),
                [statement],
                "query renderer round trip failed for seed {seed}: {query_sql}"
            );

            let expected = evaluate(&table, &select);
            let actual = execute_query(&mut database, &query_sql, seed);
            assert_eq!(
                actual, expected,
                "differential mismatch for seed {seed}:\n{query_sql}"
            );
        }
    }
}

fn generate_table(generator: &mut Generator) -> ModelTable {
    let column_count = 4 + generator.usize(MAX_COLUMNS - 3);
    let base_types = [
        DataType::Int64,
        DataType::Float64,
        DataType::Bool,
        DataType::String,
    ];
    let rotation = generator.usize(base_types.len());
    let schema = (0..column_count)
        .map(|index| ColumnDef {
            name: format!("c{index}"),
            data_type: if index < base_types.len() {
                base_types[(index + rotation) % base_types.len()]
            } else {
                base_types[generator.usize(base_types.len())]
            },
        })
        .collect::<Vec<_>>();
    let row_count = 1 + generator.usize(MAX_ROWS);
    let rows = (0..row_count)
        .map(|row| {
            schema
                .iter()
                .map(|column| generated_value(column.data_type, row, generator))
                .collect()
        })
        .collect();
    ModelTable { schema, rows }
}

fn generated_value(data_type: DataType, row: usize, generator: &mut Generator) -> Value {
    match data_type {
        DataType::Int64 => Value::Int64(generator.usize(2_001) as i64 - 1_000),
        DataType::Float64 => Value::Float64((generator.usize(2_001) as i64 - 1_000) as f64 / 8.0),
        DataType::Bool => Value::Bool(generator.boolean()),
        DataType::String => {
            let words = ["", "alpha", "comma,value", "quote'value", "two words", "z"];
            Value::String(format!(
                "{}-{}",
                words[generator.usize(words.len())],
                row % 5
            ))
        }
    }
}

fn projection_query(table: &ModelTable, generator: &mut Generator) -> Select {
    let selected_count = 1 + generator.usize(table.schema.len().min(4));
    let selected = distinct_indices(generator, table.schema.len(), selected_count);
    let items = selected
        .iter()
        .enumerate()
        .map(|(output, source)| SelectItem::Column {
            name: table.schema[*source].name.clone(),
            alias: Some(format!("out{output}")),
        })
        .collect::<Vec<_>>();
    let ordering_count = 1 + generator.usize(items.len());
    let order_by = distinct_indices(generator, items.len(), ordering_count)
        .into_iter()
        .map(|output| OrderBy {
            name: format!("out{output}"),
            descending: generator.boolean(),
        })
        .collect();
    Select {
        items,
        table: "generated".to_owned(),
        predicate: Some(generate_predicate(table, generator, MAX_PREDICATE_DEPTH)),
        group_by: Vec::new(),
        order_by,
        limit: Some(generator.usize(MAX_ROWS + 3)),
    }
}

fn global_aggregate_query(table: &ModelTable, generator: &mut Generator) -> Select {
    let numeric = columns_of_types(table, &[DataType::Int64, DataType::Float64]);
    let sum_column = numeric[generator.usize(numeric.len())];
    let avg_column = numeric[generator.usize(numeric.len())];
    let min_column = generator.usize(table.schema.len());
    let max_column = generator.usize(table.schema.len());
    let items = vec![
        aggregate(AggregateFunction::Count, None, "count"),
        aggregate(AggregateFunction::Sum, Some((table, sum_column)), "sum"),
        aggregate(AggregateFunction::Min, Some((table, min_column)), "min"),
        aggregate(AggregateFunction::Max, Some((table, max_column)), "max"),
        aggregate(AggregateFunction::Avg, Some((table, avg_column)), "avg"),
    ];
    Select {
        items,
        table: "generated".to_owned(),
        predicate: None,
        group_by: Vec::new(),
        order_by: vec![OrderBy {
            name: ["count", "sum", "min", "max", "avg"][generator.usize(5)].to_owned(),
            descending: generator.boolean(),
        }],
        limit: Some(generator.usize(3)),
    }
}

fn grouped_aggregate_query(table: &ModelTable, generator: &mut Generator) -> Select {
    let preferred_groups = columns_of_types(table, &[DataType::Bool, DataType::String]);
    let group = preferred_groups[generator.usize(preferred_groups.len())];
    let numeric = columns_of_types(table, &[DataType::Int64, DataType::Float64]);
    let sum = numeric[generator.usize(numeric.len())];
    Select {
        items: vec![
            SelectItem::Column {
                name: table.schema[group].name.clone(),
                alias: Some("key".to_owned()),
            },
            aggregate(AggregateFunction::Count, None, "count"),
            aggregate(AggregateFunction::Sum, Some((table, sum)), "total"),
        ],
        table: "generated".to_owned(),
        predicate: Some(generate_predicate(table, generator, MAX_PREDICATE_DEPTH)),
        group_by: vec![table.schema[group].name.clone()],
        order_by: vec![
            OrderBy {
                name: "total".to_owned(),
                descending: generator.boolean(),
            },
            OrderBy {
                name: "key".to_owned(),
                descending: generator.boolean(),
            },
        ],
        limit: Some(generator.usize(MAX_ROWS + 3)),
    }
}

fn aggregate(
    function: AggregateFunction,
    column: Option<(&ModelTable, usize)>,
    alias: &str,
) -> SelectItem {
    SelectItem::Aggregate {
        function,
        argument: column.map_or(AggregateArgument::Wildcard, |(table, index)| {
            AggregateArgument::Column(table.schema[index].name.clone())
        }),
        alias: Some(alias.to_owned()),
    }
}

fn distinct_indices(generator: &mut Generator, upper: usize, count: usize) -> Vec<usize> {
    let mut indices = (0..upper).collect::<Vec<_>>();
    for index in 0..count {
        let selected = index + generator.usize(upper - index);
        indices.swap(index, selected);
    }
    indices.truncate(count);
    indices
}

fn columns_of_types(table: &ModelTable, types: &[DataType]) -> Vec<usize> {
    table
        .schema
        .iter()
        .enumerate()
        .filter_map(|(index, column)| types.contains(&column.data_type).then_some(index))
        .collect()
}

fn generate_predicate(table: &ModelTable, generator: &mut Generator, depth: usize) -> Predicate {
    if depth > 0 && generator.usize(3) != 0 {
        let left = generate_predicate(table, generator, depth - 1);
        let right = generate_predicate(table, generator, depth - 1);
        if generator.boolean() {
            Predicate::And(Box::new(left), Box::new(right))
        } else {
            Predicate::Or(Box::new(left), Box::new(right))
        }
    } else {
        let source = generator.usize(table.schema.len());
        let data_type = table.schema[source].data_type;
        let compatible = columns_of_types(table, &[data_type]);
        let other = compatible[generator.usize(compatible.len())];
        let literal = generated_value(data_type, generator.usize(7), generator);
        let (left, right) = match generator.usize(3) {
            0 => (
                Operand::Column(table.schema[source].name.clone()),
                Operand::Column(table.schema[other].name.clone()),
            ),
            1 => (
                Operand::Literal(literal),
                Operand::Column(table.schema[source].name.clone()),
            ),
            _ => (
                Operand::Column(table.schema[source].name.clone()),
                Operand::Literal(literal),
            ),
        };
        let operator = [
            ComparisonOperator::Equal,
            ComparisonOperator::NotEqual,
            ComparisonOperator::Less,
            ComparisonOperator::LessOrEqual,
            ComparisonOperator::Greater,
            ComparisonOperator::GreaterOrEqual,
        ][generator.usize(6)];
        Predicate::Comparison {
            left,
            operator,
            right,
        }
    }
}

fn execute_query(database: &mut Database, query: &str, seed: u64) -> QueryResult {
    let results = database
        .execute(query)
        .unwrap_or_else(|error| panic!("query failed for seed {seed}: {error}\n{query}"));
    let [StatementResult::Query(result)] = results.as_slice() else {
        panic!("expected one query result for seed {seed}: {query}");
    };
    result.clone()
}

fn evaluate(table: &ModelTable, select: &Select) -> QueryResult {
    let matching = table
        .rows
        .iter()
        .enumerate()
        .filter(|(_, row)| {
            select
                .predicate
                .as_ref()
                .is_none_or(|predicate| evaluate_predicate(table, row, predicate))
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let columns = output_columns(table, &select.items);
    let grouped = !select.group_by.is_empty()
        || select
            .items
            .iter()
            .any(|item| matches!(item, SelectItem::Aggregate { .. }));
    let rows = if grouped {
        evaluate_grouped(table, select, &columns, &matching)
    } else {
        evaluate_projection(table, select, &columns, &matching)
    };
    QueryResult { columns, rows }
}

fn output_columns(table: &ModelTable, items: &[SelectItem]) -> Vec<ResultColumn> {
    let mut output = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard => output.extend(table.schema.iter().map(|column| ResultColumn {
                name: column.name.clone(),
                data_type: column.data_type,
            })),
            SelectItem::Column { name, alias } => {
                let source = column_index(table, name);
                output.push(ResultColumn {
                    name: alias.clone().unwrap_or_else(|| name.clone()),
                    data_type: table.schema[source].data_type,
                });
            }
            SelectItem::Aggregate {
                function,
                argument,
                alias,
            } => {
                let argument_type = match argument {
                    AggregateArgument::Wildcard => None,
                    AggregateArgument::Column(name) => {
                        Some(table.schema[column_index(table, name)].data_type)
                    }
                };
                output.push(ResultColumn {
                    name: alias.clone().expect("generated aggregates have aliases"),
                    data_type: match function {
                        AggregateFunction::Count => DataType::Int64,
                        AggregateFunction::Avg => DataType::Float64,
                        AggregateFunction::Sum
                        | AggregateFunction::Min
                        | AggregateFunction::Max => argument_type.expect("column aggregate"),
                    },
                });
            }
        }
    }
    output
}

fn evaluate_projection(
    table: &ModelTable,
    select: &Select,
    columns: &[ResultColumn],
    matching: &[usize],
) -> Vec<Vec<Value>> {
    let mut output = matching
        .iter()
        .map(|source| {
            let row = &table.rows[*source];
            let projected = select
                .items
                .iter()
                .flat_map(|item| match item {
                    SelectItem::Wildcard => row.clone(),
                    SelectItem::Column { name, .. } => vec![row[column_index(table, name)].clone()],
                    SelectItem::Aggregate { .. } => unreachable!("projection aggregate"),
                })
                .collect::<Vec<_>>();
            (*source, projected)
        })
        .collect::<Vec<_>>();
    output.sort_by(|(left_source, left), (right_source, right)| {
        compare_outputs(left, right, columns, &select.order_by)
            .then_with(|| left_source.cmp(right_source))
    });
    if let Some(limit) = select.limit {
        output.truncate(limit);
    }
    output.into_iter().map(|(_, row)| row).collect()
}

fn evaluate_grouped(
    table: &ModelTable,
    select: &Select,
    columns: &[ResultColumn],
    matching: &[usize],
) -> Vec<Vec<Value>> {
    let group_columns = select
        .group_by
        .iter()
        .map(|name| column_index(table, name))
        .collect::<Vec<_>>();
    let mut groups = Vec::<(Vec<Value>, Vec<usize>)>::new();
    if group_columns.is_empty() {
        groups.push((Vec::new(), matching.to_vec()));
    } else {
        for source in matching {
            let key = group_columns
                .iter()
                .map(|column| table.rows[*source][*column].clone())
                .collect::<Vec<_>>();
            if let Some((_, rows)) = groups
                .iter_mut()
                .find(|(existing, _)| model_slices_equal(existing, &key))
            {
                rows.push(*source);
            } else {
                groups.push((key, vec![*source]));
            }
        }
    }

    let mut output = groups
        .into_iter()
        .map(|(key, rows)| {
            let projected = select
                .items
                .iter()
                .map(|item| match item {
                    SelectItem::Column { name, .. } => {
                        let source = column_index(table, name);
                        let position = group_columns
                            .iter()
                            .position(|column| *column == source)
                            .expect("selected column is grouped");
                        key[position].clone()
                    }
                    SelectItem::Aggregate {
                        function, argument, ..
                    } => evaluate_aggregate(table, &rows, *function, argument),
                    SelectItem::Wildcard => unreachable!("generated grouping has no wildcard"),
                })
                .collect::<Vec<_>>();
            (key, projected)
        })
        .collect::<Vec<_>>();
    output.sort_by(|(left_key, left), (right_key, right)| {
        compare_outputs(left, right, columns, &select.order_by)
            .then_with(|| compare_slices(left_key, right_key))
    });
    if let Some(limit) = select.limit {
        output.truncate(limit);
    }
    output.into_iter().map(|(_, row)| row).collect()
}

fn evaluate_aggregate(
    table: &ModelTable,
    rows: &[usize],
    function: AggregateFunction,
    argument: &AggregateArgument,
) -> Value {
    let column = match argument {
        AggregateArgument::Wildcard => None,
        AggregateArgument::Column(name) => Some(column_index(table, name)),
    };
    match function {
        AggregateFunction::Count => Value::Int64(rows.len() as i64),
        AggregateFunction::Sum => match table.schema[column.expect("SUM column")].data_type {
            DataType::Int64 => Value::Int64(
                rows.iter()
                    .map(|row| match table.rows[*row][column.expect("SUM column")] {
                        Value::Int64(value) => value,
                        _ => unreachable!("typed row"),
                    })
                    .sum(),
            ),
            DataType::Float64 => Value::Float64(
                rows.iter()
                    .map(|row| match table.rows[*row][column.expect("SUM column")] {
                        Value::Float64(value) => value,
                        _ => unreachable!("typed row"),
                    })
                    .sum(),
            ),
            _ => unreachable!("generated SUM is numeric"),
        },
        AggregateFunction::Min | AggregateFunction::Max => rows
            .iter()
            .map(|row| table.rows[*row][column.expect("extrema column")].clone())
            .reduce(|left, right| {
                let order = model_cmp(&left, &right);
                if (function == AggregateFunction::Min && order == Ordering::Greater)
                    || (function == AggregateFunction::Max && order == Ordering::Less)
                {
                    right
                } else {
                    left
                }
            })
            .expect("generated global extrema is non-empty"),
        AggregateFunction::Avg => match table.schema[column.expect("AVG column")].data_type {
            DataType::Int64 => {
                let sum = rows
                    .iter()
                    .map(|row| match table.rows[*row][column.expect("AVG column")] {
                        Value::Int64(value) => i128::from(value),
                        _ => unreachable!("typed row"),
                    })
                    .sum::<i128>();
                Value::Float64(sum as f64 / rows.len() as f64)
            }
            DataType::Float64 => {
                let sum = rows
                    .iter()
                    .map(|row| match table.rows[*row][column.expect("AVG column")] {
                        Value::Float64(value) => value,
                        _ => unreachable!("typed row"),
                    })
                    .sum::<f64>();
                Value::Float64(sum / rows.len() as f64)
            }
            _ => unreachable!("generated AVG is numeric"),
        },
    }
}

fn evaluate_predicate(table: &ModelTable, row: &[Value], predicate: &Predicate) -> bool {
    match predicate {
        Predicate::Comparison {
            left,
            operator,
            right,
        } => {
            let left = operand_value(table, row, left);
            let right = operand_value(table, row, right);
            let ordering = model_cmp(left, right);
            match operator {
                ComparisonOperator::Equal => ordering == Ordering::Equal,
                ComparisonOperator::NotEqual => ordering != Ordering::Equal,
                ComparisonOperator::Less => ordering == Ordering::Less,
                ComparisonOperator::LessOrEqual => ordering != Ordering::Greater,
                ComparisonOperator::Greater => ordering == Ordering::Greater,
                ComparisonOperator::GreaterOrEqual => ordering != Ordering::Less,
            }
        }
        Predicate::And(left, right) => {
            evaluate_predicate(table, row, left) && evaluate_predicate(table, row, right)
        }
        Predicate::Or(left, right) => {
            evaluate_predicate(table, row, left) || evaluate_predicate(table, row, right)
        }
    }
}

fn operand_value<'a>(table: &ModelTable, row: &'a [Value], operand: &'a Operand) -> &'a Value {
    match operand {
        Operand::Column(name) => &row[column_index(table, name)],
        Operand::Literal(value) => value,
    }
}

fn compare_outputs(
    left: &[Value],
    right: &[Value],
    columns: &[ResultColumn],
    ordering: &[OrderBy],
) -> Ordering {
    for order in ordering {
        let output = columns
            .iter()
            .position(|column| column.name.eq_ignore_ascii_case(&order.name))
            .expect("generated ORDER BY output exists");
        let comparison = model_cmp(&left[output], &right[output]);
        if comparison != Ordering::Equal {
            return if order.descending {
                comparison.reverse()
            } else {
                comparison
            };
        }
    }
    Ordering::Equal
}

fn compare_slices(left: &[Value], right: &[Value]) -> Ordering {
    for (left, right) in left.iter().zip(right) {
        let comparison = model_cmp(left, right);
        if comparison != Ordering::Equal {
            return comparison;
        }
    }
    left.len().cmp(&right.len())
}

fn model_slices_equal(left: &[Value], right: &[Value]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| model_cmp(left, right) == Ordering::Equal)
}

fn model_cmp(left: &Value, right: &Value) -> Ordering {
    match (left, right) {
        (Value::Int64(left), Value::Int64(right)) => left.cmp(right),
        (Value::Float64(left), Value::Float64(right)) => left.total_cmp(right),
        (Value::Bool(left), Value::Bool(right)) => left.cmp(right),
        (Value::String(left), Value::String(right)) => left.cmp(right),
        _ => panic!("generator compared unlike types: {left:?} and {right:?}"),
    }
}

fn column_index(table: &ModelTable, name: &str) -> usize {
    table
        .schema
        .iter()
        .position(|column| column.name.eq_ignore_ascii_case(name))
        .expect("generated column exists")
}
