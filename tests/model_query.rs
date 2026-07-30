//! Deterministic differential tests for the SQL engine.
//!
//! The model deliberately stores rows and interprets a test-only query AST;
//! it does not reuse RustHouse's parser, column storage, or executor.

use std::cmp::Ordering;

use rusthouse::{DataType, Database, Error, QueryResult, ResultColumn, StatementResult, Value};

const BASE_SEED: u64 = 0x5eed_fade_cafe_beef;
const CASE_COUNT: usize = 256;

#[derive(Debug, Clone)]
struct Field {
    name: String,
    data_type: DataType,
}

#[derive(Debug, Clone)]
struct ModelTable {
    fields: Vec<Field>,
    rows: Vec<Vec<Cell>>,
}

#[derive(Debug, Clone)]
enum Cell {
    Int64(i64),
    Float64(f64),
    Bool(bool),
    String(String),
}

impl Cell {
    fn data_type(&self) -> DataType {
        match self {
            Self::Int64(_) => DataType::Int64,
            Self::Float64(_) => DataType::Float64,
            Self::Bool(_) => DataType::Bool,
            Self::String(_) => DataType::String,
        }
    }

    fn to_value(&self) -> Value {
        match self {
            Self::Int64(value) => Value::Int64(*value),
            Self::Float64(value) => Value::Float64(*value),
            Self::Bool(value) => Value::Bool(*value),
            Self::String(value) => Value::String(value.clone()),
        }
    }

    fn to_sql(&self) -> String {
        match self {
            Self::Int64(value) => value.to_string(),
            Self::Float64(value) => {
                let rendered = value.to_string();
                if rendered.contains(['.', 'e', 'E']) {
                    rendered
                } else {
                    format!("{rendered}.0")
                }
            }
            Self::Bool(value) => value.to_string(),
            Self::String(value) => format!("'{}'", value.replace('\'', "''")),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Compare {
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
}

impl Compare {
    fn sql(self) -> &'static str {
        match self {
            Self::Equal => "=",
            Self::NotEqual => "!=",
            Self::Less => "<",
            Self::LessOrEqual => "<=",
            Self::Greater => ">",
            Self::GreaterOrEqual => ">=",
        }
    }
}

#[derive(Debug, Clone)]
enum Operand {
    Column(usize),
    Literal(Cell),
}

#[derive(Debug, Clone)]
enum Predicate {
    Comparison {
        left: Operand,
        operator: Compare,
        right: Operand,
    },
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
}

#[derive(Debug, Clone, Copy)]
enum Aggregate {
    Count,
    Sum,
    Min,
    Max,
    Avg,
}

impl Aggregate {
    fn name(self) -> &'static str {
        match self {
            Self::Count => "COUNT",
            Self::Sum => "SUM",
            Self::Min => "MIN",
            Self::Max => "MAX",
            Self::Avg => "AVG",
        }
    }
}

#[derive(Debug, Clone)]
enum SelectItem {
    Column(usize),
    Aggregate {
        function: Aggregate,
        argument: Option<usize>,
    },
}

#[derive(Debug, Clone, Copy)]
struct Order {
    output: usize,
    descending: bool,
}

#[derive(Debug, Clone)]
struct Query {
    items: Vec<SelectItem>,
    predicate: Option<Predicate>,
    group_by: Vec<usize>,
    order_by: Vec<Order>,
    limit: Option<usize>,
}

#[derive(Debug, Default, Clone, Copy)]
struct Dimensions {
    boundary_integer: bool,
    mixed_numeric: bool,
    escaped_string: bool,
    empty_input: bool,
    ordering_tie: bool,
    invalid_query: bool,
}

impl Dimensions {
    fn include(&mut self, other: Self) {
        self.boundary_integer |= other.boundary_integer;
        self.mixed_numeric |= other.mixed_numeric;
        self.escaped_string |= other.escaped_string;
        self.empty_input |= other.empty_input;
        self.ordering_tie |= other.ordering_tie;
        self.invalid_query |= other.invalid_query;
    }
}

struct GeneratedCase {
    table: ModelTable,
    query: Query,
    dimensions: Dimensions,
}

#[derive(Debug, Clone)]
enum AggregateState {
    Count(i64),
    SumInt(i64),
    SumFloat(f64),
    Min(Option<Cell>),
    Max(Option<Cell>),
    AvgInt { sum: i128, count: u64 },
    AvgFloat { sum: f64, count: u64 },
}

#[derive(Debug)]
struct Group {
    key: Vec<Cell>,
    states: Vec<AggregateState>,
}

#[test]
fn fixed_seed_queries_match_the_row_model() {
    let mut covered = Dimensions::default();

    for ordinal in 0..CASE_COUNT {
        let seed = BASE_SEED.wrapping_add((ordinal as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15));
        let generated = generate_case(seed, ordinal);
        covered.include(generated.dimensions);

        let mut database = Database::new();
        let setup = setup_sql(&generated.table);
        database.execute(&setup).unwrap_or_else(|error| {
            panic!("generated setup failed for seed {seed:#x}: {error}\n{setup}")
        });

        let sql = query_sql(&generated.table, &generated.query);
        let expected = execute_model(&generated.table, &generated.query);
        let actual = execute_rusthouse(&mut database, &sql);
        assert_eq!(
            actual, expected,
            "model mismatch for seed {seed:#x} (case {ordinal})\nsetup: {setup}\nquery: {sql}"
        );
    }

    assert!(
        covered.boundary_integer,
        "boundary integers were not generated"
    );
    assert!(
        covered.mixed_numeric,
        "mixed numeric predicates were not generated"
    );
    assert!(covered.escaped_string, "escaped strings were not generated");
    assert!(covered.empty_input, "empty inputs were not generated");
    assert!(covered.ordering_tie, "ordering ties were not generated");
    assert!(covered.invalid_query, "query errors were not generated");
}

fn execute_rusthouse(database: &mut Database, sql: &str) -> Result<QueryResult, Error> {
    database.execute(sql).map(|results| {
        let [StatementResult::Query(result)] = results.as_slice() else {
            panic!("one query result expected")
        };
        result.clone()
    })
}

fn execute_model(table: &ModelTable, query: &Query) -> Result<QueryResult, Error> {
    if let Some(predicate) = &query.predicate {
        validate_predicate(table, predicate)?;
    }
    validate_aggregates(table, &query.items)?;

    let columns = query
        .items
        .iter()
        .enumerate()
        .map(|(index, item)| ResultColumn {
            name: format!("o{index}"),
            data_type: output_type(table, item),
        })
        .collect::<Vec<_>>();
    let matching = table
        .rows
        .iter()
        .enumerate()
        .filter(|(_, row)| {
            query
                .predicate
                .as_ref()
                .is_none_or(|predicate| evaluate_predicate(predicate, row))
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();

    let grouped = !query.group_by.is_empty()
        || query
            .items
            .iter()
            .any(|item| matches!(item, SelectItem::Aggregate { .. }));
    let rows = if grouped {
        execute_grouped_model(table, query, &matching)?
    } else {
        execute_projection_model(table, query, matching)
    };
    Ok(QueryResult { columns, rows })
}

fn execute_projection_model(
    table: &ModelTable,
    query: &Query,
    mut matching: Vec<usize>,
) -> Vec<Vec<Value>> {
    if !query.order_by.is_empty() {
        matching.sort_unstable_by(|left, right| {
            for order in &query.order_by {
                let SelectItem::Column(column) = query.items[order.output] else {
                    unreachable!("an ungrouped query only projects columns")
                };
                let comparison =
                    storage_cmp(&table.rows[*left][column], &table.rows[*right][column]);
                if comparison != Ordering::Equal {
                    return descending(comparison, order.descending);
                }
            }
            left.cmp(right)
        });
    }
    if let Some(limit) = query.limit {
        matching.truncate(limit);
    }
    matching
        .into_iter()
        .map(|row| {
            query
                .items
                .iter()
                .map(|item| {
                    let SelectItem::Column(column) = item else {
                        unreachable!("an ungrouped query only projects columns")
                    };
                    table.rows[row][*column].to_value()
                })
                .collect()
        })
        .collect()
}

fn execute_grouped_model(
    table: &ModelTable,
    query: &Query,
    matching: &[usize],
) -> Result<Vec<Vec<Value>>, Error> {
    let aggregate_items = query
        .items
        .iter()
        .filter_map(|item| match item {
            SelectItem::Aggregate { function, argument } => Some((*function, *argument)),
            SelectItem::Column(_) => None,
        })
        .collect::<Vec<_>>();
    let mut groups = if query.group_by.is_empty() {
        vec![Group {
            key: Vec::new(),
            states: new_states(table, &aggregate_items),
        }]
    } else {
        Vec::new()
    };

    for row_index in matching {
        let key = query
            .group_by
            .iter()
            .map(|column| table.rows[*row_index][*column].clone())
            .collect::<Vec<_>>();
        let group_index = groups
            .iter()
            .position(|group| key_eq(&group.key, &key))
            .unwrap_or_else(|| {
                groups.push(Group {
                    key,
                    states: new_states(table, &aggregate_items),
                });
                groups.len() - 1
            });
        for (state, (_, argument)) in groups[group_index].states.iter_mut().zip(&aggregate_items) {
            let value = argument.map(|column| &table.rows[*row_index][column]);
            update_state(state, value)?;
        }
    }

    let mut aggregates = vec![vec![None; groups.len()]; aggregate_items.len()];
    for (aggregate, finished) in aggregates.iter_mut().enumerate() {
        for (group, values) in groups.iter().enumerate() {
            finished[group] = Some(finish_state(values.states[aggregate].clone())?);
        }
    }
    let mut rows = groups
        .iter()
        .enumerate()
        .map(|(group, data)| {
            let mut aggregate = 0;
            query
                .items
                .iter()
                .map(|item| match item {
                    SelectItem::Column(column) => {
                        let position = query
                            .group_by
                            .iter()
                            .position(|grouped| grouped == column)
                            .expect("generated projected column is grouped");
                        data.key[position].to_value()
                    }
                    SelectItem::Aggregate { .. } => {
                        let value = aggregates[aggregate][group]
                            .as_ref()
                            .expect("aggregate state was finished")
                            .to_value();
                        aggregate += 1;
                        value
                    }
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let mut selected = (0..groups.len()).collect::<Vec<_>>();
    selected.sort_unstable_by(|left, right| {
        for order in &query.order_by {
            let comparison = value_cmp(&rows[*left][order.output], &rows[*right][order.output]);
            if comparison != Ordering::Equal {
                return descending(comparison, order.descending);
            }
        }
        key_cmp(&groups[*left].key, &groups[*right].key)
    });
    if let Some(limit) = query.limit {
        selected.truncate(limit);
    }
    Ok(selected
        .into_iter()
        .map(|group| std::mem::take(&mut rows[group]))
        .collect())
}

fn validate_predicate(table: &ModelTable, predicate: &Predicate) -> Result<(), Error> {
    match predicate {
        Predicate::Comparison { left, right, .. } => {
            let left = operand_type(table, left);
            let right = operand_type(table, right);
            if left != right && !is_numeric(left, right) {
                return Err(Error::TypeMismatch {
                    context: "WHERE comparison".to_owned(),
                    expected: left.to_string(),
                    actual: right.to_string(),
                });
            }
            Ok(())
        }
        Predicate::And(left, right) | Predicate::Or(left, right) => {
            validate_predicate(table, left)?;
            validate_predicate(table, right)
        }
    }
}

fn validate_aggregates(table: &ModelTable, items: &[SelectItem]) -> Result<(), Error> {
    for item in items {
        let SelectItem::Aggregate {
            function,
            argument: Some(column),
        } = item
        else {
            continue;
        };
        let input = table.fields[*column].data_type;
        if matches!(function, Aggregate::Sum | Aggregate::Avg)
            && !matches!(input, DataType::Int64 | DataType::Float64)
        {
            return Err(Error::TypeMismatch {
                context: format!("{} argument", function.name()),
                expected: "Int64 or Float64".to_owned(),
                actual: input.to_string(),
            });
        }
    }
    Ok(())
}

fn evaluate_predicate(predicate: &Predicate, row: &[Cell]) -> bool {
    match predicate {
        Predicate::Comparison {
            left,
            operator,
            right,
        } => {
            let comparison = sql_cmp(operand_value(left, row), operand_value(right, row));
            match operator {
                Compare::Equal => comparison == Ordering::Equal,
                Compare::NotEqual => comparison != Ordering::Equal,
                Compare::Less => comparison == Ordering::Less,
                Compare::LessOrEqual => comparison != Ordering::Greater,
                Compare::Greater => comparison == Ordering::Greater,
                Compare::GreaterOrEqual => comparison != Ordering::Less,
            }
        }
        Predicate::And(left, right) => {
            evaluate_predicate(left, row) && evaluate_predicate(right, row)
        }
        Predicate::Or(left, right) => {
            evaluate_predicate(left, row) || evaluate_predicate(right, row)
        }
    }
}

fn new_states(table: &ModelTable, items: &[(Aggregate, Option<usize>)]) -> Vec<AggregateState> {
    items
        .iter()
        .map(|(function, argument)| match function {
            Aggregate::Count => AggregateState::Count(0),
            Aggregate::Sum
                if table.fields[argument.expect("SUM column")].data_type == DataType::Int64 =>
            {
                AggregateState::SumInt(0)
            }
            Aggregate::Sum => AggregateState::SumFloat(0.0),
            Aggregate::Min => AggregateState::Min(None),
            Aggregate::Max => AggregateState::Max(None),
            Aggregate::Avg
                if table.fields[argument.expect("AVG column")].data_type == DataType::Int64 =>
            {
                AggregateState::AvgInt { sum: 0, count: 0 }
            }
            Aggregate::Avg => AggregateState::AvgFloat { sum: 0.0, count: 0 },
        })
        .collect()
}

fn update_state(state: &mut AggregateState, value: Option<&Cell>) -> Result<(), Error> {
    match state {
        AggregateState::Count(count) => {
            *count = count
                .checked_add(1)
                .ok_or_else(|| Error::NumericOverflow("COUNT".to_owned()))?;
        }
        AggregateState::SumInt(sum) => {
            let Some(Cell::Int64(value)) = value else {
                unreachable!("SUM input was validated")
            };
            *sum = sum
                .checked_add(*value)
                .ok_or_else(|| Error::NumericOverflow("SUM(Int64)".to_owned()))?;
        }
        AggregateState::SumFloat(sum) => {
            let Some(Cell::Float64(value)) = value else {
                unreachable!("SUM input was validated")
            };
            *sum += value;
            if !sum.is_finite() {
                return Err(Error::NumericOverflow("SUM(Float64)".to_owned()));
            }
        }
        AggregateState::Min(current) => {
            let value = value.expect("MIN column");
            if current
                .as_ref()
                .is_none_or(|existing| storage_cmp(value, existing) == Ordering::Less)
            {
                *current = Some(value.clone());
            }
        }
        AggregateState::Max(current) => {
            let value = value.expect("MAX column");
            if current
                .as_ref()
                .is_none_or(|existing| storage_cmp(value, existing) == Ordering::Greater)
            {
                *current = Some(value.clone());
            }
        }
        AggregateState::AvgInt { sum, count } => {
            let Some(Cell::Int64(value)) = value else {
                unreachable!("AVG input was validated")
            };
            *sum = sum
                .checked_add(i128::from(*value))
                .ok_or_else(|| Error::NumericOverflow("AVG(Int64) sum".to_owned()))?;
            *count = count
                .checked_add(1)
                .ok_or_else(|| Error::NumericOverflow("AVG count".to_owned()))?;
        }
        AggregateState::AvgFloat { sum, count } => {
            let Some(Cell::Float64(value)) = value else {
                unreachable!("AVG input was validated")
            };
            *sum += value;
            *count = count
                .checked_add(1)
                .ok_or_else(|| Error::NumericOverflow("AVG count".to_owned()))?;
            if !sum.is_finite() {
                return Err(Error::NumericOverflow("AVG(Float64) sum".to_owned()));
            }
        }
    }
    Ok(())
}

fn finish_state(state: AggregateState) -> Result<Cell, Error> {
    match state {
        AggregateState::Count(value) | AggregateState::SumInt(value) => Ok(Cell::Int64(value)),
        AggregateState::SumFloat(value) => Ok(Cell::Float64(value)),
        AggregateState::Min(Some(value)) | AggregateState::Max(Some(value)) => Ok(value),
        AggregateState::AvgInt { sum, count } if count > 0 => {
            Ok(Cell::Float64(sum as f64 / count as f64))
        }
        AggregateState::AvgFloat { sum, count } if count > 0 => {
            Ok(Cell::Float64(sum / count as f64))
        }
        AggregateState::Min(None) => Err(Error::InvalidQuery(
            "MIN is undefined for an empty input".to_owned(),
        )),
        AggregateState::Max(None) => Err(Error::InvalidQuery(
            "MAX is undefined for an empty input".to_owned(),
        )),
        AggregateState::AvgInt { .. } | AggregateState::AvgFloat { .. } => Err(
            Error::InvalidQuery("AVG is undefined for an empty input".to_owned()),
        ),
    }
}

fn generate_case(seed: u64, ordinal: usize) -> GeneratedCase {
    let mut rng = Rng::new(seed);
    let fields = generate_schema(&mut rng);
    let row_count = if ordinal.is_multiple_of(19) {
        0
    } else if ordinal % 7 == 5 {
        12
    } else {
        rng.usize(16)
    };
    let rows = (0..row_count)
        .map(|row| {
            fields
                .iter()
                .enumerate()
                .map(|(column, field)| generated_value(field.data_type, row, column, ordinal))
                .collect()
        })
        .collect::<Vec<_>>();
    let table = ModelTable { fields, rows };

    let mut dimensions = Dimensions {
        boundary_integer: table.rows.iter().flatten().any(|value| {
            matches!(
                value,
                Cell::Int64(i64::MIN | i64::MAX | 9_007_199_254_740_993)
            )
        }),
        escaped_string: table
            .rows
            .iter()
            .flatten()
            .any(|value| matches!(value, Cell::String(text) if text.contains('\''))),
        empty_input: table.rows.is_empty(),
        ..Dimensions::default()
    };

    let query = if table.rows.is_empty() && ordinal.is_multiple_of(2) {
        empty_aggregate_query(&table, ordinal)
    } else {
        match ordinal % 7 {
            0 => projection_query(&table, &mut rng, false, ordinal, &mut dimensions),
            1 | 6 => grouped_query(&table, &mut rng, ordinal, &mut dimensions),
            2 => global_query(&table, &mut rng, ordinal, &mut dimensions),
            3 => invalid_predicate_query(&table, &mut rng, &mut dimensions),
            4 => invalid_aggregate_query(&table, &mut rng, &mut dimensions),
            5 => projection_query(&table, &mut rng, true, ordinal, &mut dimensions),
            _ => unreachable!(),
        }
    };
    GeneratedCase {
        table,
        query,
        dimensions,
    }
}

fn generate_schema(rng: &mut Rng) -> Vec<Field> {
    let mut types = vec![
        DataType::Int64,
        DataType::Float64,
        DataType::Bool,
        DataType::String,
    ];
    rng.shuffle(&mut types);
    for _ in 0..rng.usize(3) {
        types.push(rng.data_type());
    }
    types
        .into_iter()
        .enumerate()
        .map(|(index, data_type)| Field {
            name: format!("c{index}"),
            data_type,
        })
        .collect()
}

fn generated_value(data_type: DataType, row: usize, column: usize, ordinal: usize) -> Cell {
    const INTS: [i64; 12] = [
        i64::MIN,
        i64::MIN + 1,
        -9_007_199_254_740_993,
        -9_007_199_254_740_992,
        -1,
        0,
        1,
        7,
        9_007_199_254_740_992,
        9_007_199_254_740_993,
        i64::MAX - 1,
        i64::MAX,
    ];
    const FLOATS: [f64; 12] = [
        -9_223_372_036_854_775_808.0,
        -9_007_199_254_740_992.0,
        -1.5,
        -0.0,
        0.0,
        0.5,
        1.0,
        7.25,
        9_007_199_254_740_992.0,
        9_223_372_036_854_774_784.0,
        9_223_372_036_854_775_808.0,
        1.0e100,
    ];
    const STRINGS: [&str; 8] = [
        "",
        "alpha",
        "O'Brien",
        "two '' quotes",
        "comma,value",
        "line\nbreak",
        "zeta",
        "alpha",
    ];
    let offset = ordinal.wrapping_add(column * 3);
    match data_type {
        DataType::Int64 => Cell::Int64(INTS[(row + offset) % INTS.len()]),
        DataType::Float64 => Cell::Float64(FLOATS[(row + offset) % FLOATS.len()]),
        DataType::Bool => Cell::Bool((row + column).is_multiple_of(2)),
        DataType::String => Cell::String(STRINGS[(row + offset) % STRINGS.len()].to_owned()),
    }
}

fn projection_query(
    table: &ModelTable,
    rng: &mut Rng,
    force_ties: bool,
    ordinal: usize,
    dimensions: &mut Dimensions,
) -> Query {
    let mut columns = (0..table.fields.len()).collect::<Vec<_>>();
    rng.shuffle(&mut columns);
    columns.truncate(1 + rng.usize(columns.len()));
    if force_ties {
        let boolean = column_of_type(table, DataType::Bool);
        if !columns.contains(&boolean) {
            columns.push(boolean);
        }
        let output = columns
            .iter()
            .position(|column| *column == boolean)
            .expect("Boolean projection");
        dimensions.ordering_tie = table.rows.len() > 2;
        return Query {
            items: columns.into_iter().map(SelectItem::Column).collect(),
            predicate: None,
            group_by: Vec::new(),
            order_by: vec![Order {
                output,
                descending: rng.bool(),
            }],
            limit: generated_limit(rng, table.rows.len()),
        };
    }

    let predicate = Some(valid_predicate(table, rng, ordinal, dimensions));
    let item_count = columns.len();
    Query {
        items: columns.into_iter().map(SelectItem::Column).collect(),
        predicate,
        group_by: Vec::new(),
        order_by: generated_order(rng, item_count),
        limit: generated_limit(rng, table.rows.len()),
    }
}

fn grouped_query(
    table: &ModelTable,
    rng: &mut Rng,
    ordinal: usize,
    dimensions: &mut Dimensions,
) -> Query {
    let mut group_by = (0..table.fields.len()).collect::<Vec<_>>();
    rng.shuffle(&mut group_by);
    group_by.truncate(1 + rng.usize(2.min(group_by.len())));
    let mut items = group_by
        .iter()
        .copied()
        .map(SelectItem::Column)
        .collect::<Vec<_>>();
    for _ in 0..1 + rng.usize(4) {
        items.push(valid_aggregate_item(table, rng));
    }
    let item_count = items.len();
    Query {
        items,
        predicate: Some(valid_predicate(table, rng, ordinal, dimensions)),
        group_by,
        order_by: generated_order(rng, item_count),
        limit: generated_limit(rng, table.rows.len()),
    }
}

fn global_query(
    table: &ModelTable,
    rng: &mut Rng,
    ordinal: usize,
    dimensions: &mut Dimensions,
) -> Query {
    let items = (0..1 + rng.usize(5))
        .map(|_| valid_aggregate_item(table, rng))
        .collect::<Vec<_>>();
    let item_count = items.len();
    Query {
        items,
        predicate: Some(valid_predicate(table, rng, ordinal, dimensions)),
        group_by: Vec::new(),
        order_by: generated_order(rng, item_count),
        limit: generated_limit(rng, 1),
    }
}

fn invalid_predicate_query(
    table: &ModelTable,
    rng: &mut Rng,
    dimensions: &mut Dimensions,
) -> Query {
    dimensions.invalid_query = true;
    let column = column_of_type(table, DataType::String);
    Query {
        items: vec![SelectItem::Column(column)],
        predicate: Some(Predicate::Comparison {
            left: Operand::Column(column),
            operator: rng.compare(),
            right: Operand::Literal(Cell::Int64(1)),
        }),
        group_by: Vec::new(),
        order_by: Vec::new(),
        limit: None,
    }
}

fn invalid_aggregate_query(
    table: &ModelTable,
    rng: &mut Rng,
    dimensions: &mut Dimensions,
) -> Query {
    dimensions.invalid_query = true;
    let data_type = if rng.bool() {
        DataType::Bool
    } else {
        DataType::String
    };
    Query {
        items: vec![SelectItem::Aggregate {
            function: if rng.bool() {
                Aggregate::Sum
            } else {
                Aggregate::Avg
            },
            argument: Some(column_of_type(table, data_type)),
        }],
        predicate: None,
        group_by: Vec::new(),
        order_by: Vec::new(),
        limit: None,
    }
}

fn empty_aggregate_query(table: &ModelTable, ordinal: usize) -> Query {
    let function = match (ordinal / 2) % 3 {
        0 => Aggregate::Min,
        1 => Aggregate::Max,
        _ => Aggregate::Avg,
    };
    let argument = match function {
        Aggregate::Avg => column_of_type(table, DataType::Int64),
        Aggregate::Min | Aggregate::Max => ordinal % table.fields.len(),
        Aggregate::Count | Aggregate::Sum => unreachable!(),
    };
    Query {
        items: vec![SelectItem::Aggregate {
            function,
            argument: Some(argument),
        }],
        predicate: None,
        group_by: Vec::new(),
        order_by: Vec::new(),
        limit: None,
    }
}

fn valid_aggregate_item(table: &ModelTable, rng: &mut Rng) -> SelectItem {
    let function = rng.aggregate();
    let argument = match function {
        Aggregate::Count if rng.bool() => None,
        Aggregate::Count | Aggregate::Min | Aggregate::Max => Some(rng.usize(table.fields.len())),
        Aggregate::Sum | Aggregate::Avg => {
            let data_type = if rng.bool() {
                DataType::Int64
            } else {
                DataType::Float64
            };
            Some(column_of_type(table, data_type))
        }
    };
    SelectItem::Aggregate { function, argument }
}

fn valid_predicate(
    table: &ModelTable,
    rng: &mut Rng,
    ordinal: usize,
    dimensions: &mut Dimensions,
) -> Predicate {
    let terms = 1 + rng.usize(3);
    let mut predicate = predicate_term(table, rng, ordinal.is_multiple_of(4), dimensions);
    for _ in 1..terms {
        let right = predicate_term(table, rng, false, dimensions);
        predicate = if rng.bool() {
            Predicate::And(Box::new(predicate), Box::new(right))
        } else {
            Predicate::Or(Box::new(predicate), Box::new(right))
        };
    }
    predicate
}

fn predicate_term(
    table: &ModelTable,
    rng: &mut Rng,
    force_mixed_numeric: bool,
    dimensions: &mut Dimensions,
) -> Predicate {
    let column = if force_mixed_numeric {
        column_of_type(table, DataType::Int64)
    } else {
        rng.usize(table.fields.len())
    };
    let column_type = table.fields[column].data_type;
    let literal_type = if force_mixed_numeric {
        dimensions.mixed_numeric = true;
        DataType::Float64
    } else if matches!(column_type, DataType::Int64 | DataType::Float64) && rng.bool() {
        dimensions.mixed_numeric = true;
        if column_type == DataType::Int64 {
            DataType::Float64
        } else {
            DataType::Int64
        }
    } else {
        column_type
    };
    let literal = predicate_literal(literal_type, rng);
    let (left, right) = if rng.bool() {
        (Operand::Column(column), Operand::Literal(literal))
    } else {
        (Operand::Literal(literal), Operand::Column(column))
    };
    Predicate::Comparison {
        left,
        operator: rng.compare(),
        right,
    }
}

fn predicate_literal(data_type: DataType, rng: &mut Rng) -> Cell {
    const INTS: [i64; 9] = [
        i64::MIN,
        -9_007_199_254_740_993,
        -1,
        0,
        1,
        7,
        9_007_199_254_740_992,
        9_007_199_254_740_993,
        i64::MAX,
    ];
    const FLOATS: [f64; 9] = [
        -9_223_372_036_854_775_808.0,
        -9_007_199_254_740_992.0,
        -1.5,
        0.0,
        1.0,
        7.25,
        9_007_199_254_740_992.0,
        9_223_372_036_854_774_784.0,
        9_223_372_036_854_775_808.0,
    ];
    const STRINGS: [&str; 5] = ["", "alpha", "O'Brien", "line\nbreak", "zeta"];
    match data_type {
        DataType::Int64 => Cell::Int64(INTS[rng.usize(INTS.len())]),
        DataType::Float64 => Cell::Float64(FLOATS[rng.usize(FLOATS.len())]),
        DataType::Bool => Cell::Bool(rng.bool()),
        DataType::String => Cell::String(STRINGS[rng.usize(STRINGS.len())].to_owned()),
    }
}

fn generated_order(rng: &mut Rng, item_count: usize) -> Vec<Order> {
    if rng.usize(4) == 0 {
        return Vec::new();
    }
    let mut outputs = (0..item_count).collect::<Vec<_>>();
    rng.shuffle(&mut outputs);
    outputs.truncate(1 + rng.usize(2.min(item_count)));
    outputs
        .into_iter()
        .map(|output| Order {
            output,
            descending: rng.bool(),
        })
        .collect()
}

fn generated_limit(rng: &mut Rng, row_count: usize) -> Option<usize> {
    match rng.usize(4) {
        0 => None,
        1 => Some(0),
        _ => Some(rng.usize(row_count.saturating_add(3))),
    }
}

fn setup_sql(table: &ModelTable) -> String {
    let fields = table
        .fields
        .iter()
        .map(|field| format!("{} {}", field.name, field.data_type))
        .collect::<Vec<_>>()
        .join(", ");
    let mut sql = format!("CREATE TABLE model ({fields});");
    if !table.rows.is_empty() {
        let rows = table
            .rows
            .iter()
            .map(|row| {
                format!(
                    "({})",
                    row.iter().map(Cell::to_sql).collect::<Vec<_>>().join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        sql.push_str(&format!(" INSERT INTO model VALUES {rows};"));
    }
    sql
}

fn query_sql(table: &ModelTable, query: &Query) -> String {
    let items = query
        .items
        .iter()
        .enumerate()
        .map(|(index, item)| match item {
            SelectItem::Column(column) => format!("{} AS o{index}", table.fields[*column].name),
            SelectItem::Aggregate { function, argument } => {
                let argument = argument
                    .map(|column| table.fields[column].name.as_str())
                    .unwrap_or("*");
                format!("{}({argument}) AS o{index}", function.name())
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let mut sql = format!("SELECT {items} FROM model");
    if let Some(predicate) = &query.predicate {
        sql.push_str(" WHERE ");
        sql.push_str(&predicate_sql(table, predicate));
    }
    if !query.group_by.is_empty() {
        sql.push_str(" GROUP BY ");
        sql.push_str(
            &query
                .group_by
                .iter()
                .map(|column| table.fields[*column].name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    if !query.order_by.is_empty() {
        sql.push_str(" ORDER BY ");
        sql.push_str(
            &query
                .order_by
                .iter()
                .map(|order| {
                    format!(
                        "o{} {}",
                        order.output,
                        if order.descending { "DESC" } else { "ASC" }
                    )
                })
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    if let Some(limit) = query.limit {
        sql.push_str(&format!(" LIMIT {limit}"));
    }
    sql
}

fn predicate_sql(table: &ModelTable, predicate: &Predicate) -> String {
    match predicate {
        Predicate::Comparison {
            left,
            operator,
            right,
        } => format!(
            "({} {} {})",
            operand_sql(table, left),
            operator.sql(),
            operand_sql(table, right)
        ),
        Predicate::And(left, right) => format!(
            "({} AND {})",
            predicate_sql(table, left),
            predicate_sql(table, right)
        ),
        Predicate::Or(left, right) => format!(
            "({} OR {})",
            predicate_sql(table, left),
            predicate_sql(table, right)
        ),
    }
}

fn operand_sql(table: &ModelTable, operand: &Operand) -> String {
    match operand {
        Operand::Column(column) => table.fields[*column].name.clone(),
        Operand::Literal(value) => value.to_sql(),
    }
}

fn output_type(table: &ModelTable, item: &SelectItem) -> DataType {
    match item {
        SelectItem::Column(column) => table.fields[*column].data_type,
        SelectItem::Aggregate {
            function: Aggregate::Count,
            ..
        } => DataType::Int64,
        SelectItem::Aggregate {
            function: Aggregate::Avg,
            ..
        } => DataType::Float64,
        SelectItem::Aggregate {
            argument: Some(column),
            ..
        } => table.fields[*column].data_type,
        SelectItem::Aggregate { argument: None, .. } => {
            unreachable!("only COUNT accepts a wildcard")
        }
    }
}

fn operand_type(table: &ModelTable, operand: &Operand) -> DataType {
    match operand {
        Operand::Column(column) => table.fields[*column].data_type,
        Operand::Literal(value) => value.data_type(),
    }
}

fn operand_value<'a>(operand: &'a Operand, row: &'a [Cell]) -> &'a Cell {
    match operand {
        Operand::Column(column) => &row[*column],
        Operand::Literal(value) => value,
    }
}

fn is_numeric(left: DataType, right: DataType) -> bool {
    matches!(
        (left, right),
        (DataType::Int64, DataType::Float64) | (DataType::Float64, DataType::Int64)
    )
}

fn sql_cmp(left: &Cell, right: &Cell) -> Ordering {
    match (left, right) {
        (Cell::Int64(left), Cell::Float64(right)) => int_float_cmp(*left, *right),
        (Cell::Float64(left), Cell::Int64(right)) => int_float_cmp(*right, *left).reverse(),
        _ => storage_cmp(left, right),
    }
}

fn int_float_cmp(integer: i64, float: f64) -> Ordering {
    const I64_UPPER_EXCLUSIVE: f64 = 9_223_372_036_854_775_808.0;
    if float >= I64_UPPER_EXCLUSIVE {
        return Ordering::Less;
    }
    if float < i64::MIN as f64 {
        return Ordering::Greater;
    }
    let truncated = float.trunc() as i64;
    match integer.cmp(&truncated) {
        Ordering::Equal => (truncated as f64)
            .partial_cmp(&float)
            .expect("generated floats are finite"),
        ordering => ordering,
    }
}

fn storage_cmp(left: &Cell, right: &Cell) -> Ordering {
    match (left, right) {
        (Cell::Int64(left), Cell::Int64(right)) => left.cmp(right),
        (Cell::Float64(left), Cell::Float64(right)) if left == right => Ordering::Equal,
        (Cell::Float64(left), Cell::Float64(right)) => left.total_cmp(right),
        (Cell::Bool(left), Cell::Bool(right)) => left.cmp(right),
        (Cell::String(left), Cell::String(right)) => left.cmp(right),
        _ => unreachable!("storage comparisons use one declared type"),
    }
}

fn value_cmp(left: &Value, right: &Value) -> Ordering {
    match (left, right) {
        (Value::Int64(left), Value::Int64(right)) => left.cmp(right),
        (Value::Float64(left), Value::Float64(right)) if left == right => Ordering::Equal,
        (Value::Float64(left), Value::Float64(right)) => left.total_cmp(right),
        (Value::Bool(left), Value::Bool(right)) => left.cmp(right),
        (Value::String(left), Value::String(right)) => left.cmp(right),
        _ => unreachable!("an output column has one declared type"),
    }
}

fn key_eq(left: &[Cell], right: &[Cell]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| storage_cmp(left, right) == Ordering::Equal)
}

fn key_cmp(left: &[Cell], right: &[Cell]) -> Ordering {
    left.iter()
        .zip(right)
        .map(|(left, right)| storage_cmp(left, right))
        .find(|ordering| *ordering != Ordering::Equal)
        .unwrap_or_else(|| left.len().cmp(&right.len()))
}

fn descending(ordering: Ordering, descending: bool) -> Ordering {
    if descending {
        ordering.reverse()
    } else {
        ordering
    }
}

fn column_of_type(table: &ModelTable, data_type: DataType) -> usize {
    table
        .fields
        .iter()
        .position(|field| field.data_type == data_type)
        .expect("every generated schema has every data type")
}

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0 = self.0.wrapping_mul(0x2545_f491_4f6c_dd1d);
        self.0
    }

    fn usize(&mut self, upper: usize) -> usize {
        assert!(upper > 0);
        (self.next() as usize) % upper
    }

    fn bool(&mut self) -> bool {
        self.next() & 1 == 0
    }

    fn shuffle<T>(&mut self, values: &mut [T]) {
        for index in (1..values.len()).rev() {
            values.swap(index, self.usize(index + 1));
        }
    }

    fn data_type(&mut self) -> DataType {
        [
            DataType::Int64,
            DataType::Float64,
            DataType::Bool,
            DataType::String,
        ][self.usize(4)]
    }

    fn compare(&mut self) -> Compare {
        [
            Compare::Equal,
            Compare::NotEqual,
            Compare::Less,
            Compare::LessOrEqual,
            Compare::Greater,
            Compare::GreaterOrEqual,
        ][self.usize(6)]
    }

    fn aggregate(&mut self) -> Aggregate {
        [
            Aggregate::Count,
            Aggregate::Sum,
            Aggregate::Min,
            Aggregate::Max,
            Aggregate::Avg,
        ][self.usize(5)]
    }
}
