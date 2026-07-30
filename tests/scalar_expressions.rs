use rusthouse::{DataType, Database, Error, QueryResult, StatementResult, Value};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    match database
        .execute(sql)
        .expect("query succeeds")
        .into_iter()
        .last()
        .expect("statement result")
    {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

fn sales_database() -> Database {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE sales (
                region String, price Int64, quantity Int64, adjustment Float64
             );
             INSERT INTO sales VALUES
                ('west', 10, 2, 0.5),
                ('east', 4, 5, 1.25),
                ('west', 3, 4, 0.25);",
        )
        .expect("setup succeeds");
    database
}

#[test]
fn projections_apply_precedence_parentheses_negation_and_numeric_promotion() {
    let mut database = sales_database();
    let result = query(
        &mut database,
        "SELECT price + quantity * 2 AS precedence,
                (price + quantity) * 2 AS parenthesized,
                -quantity AS negative,
                price / quantity AS integer_ratio,
                price + adjustment AS mixed
         FROM sales
         WHERE (price * quantity) >= 12
         ORDER BY mixed DESC;",
    );

    assert_eq!(
        result
            .columns
            .iter()
            .map(|column| (&column.name, column.data_type))
            .collect::<Vec<_>>(),
        vec![
            (&"precedence".to_owned(), DataType::Int64),
            (&"parenthesized".to_owned(), DataType::Int64),
            (&"negative".to_owned(), DataType::Int64),
            (&"integer_ratio".to_owned(), DataType::Int64),
            (&"mixed".to_owned(), DataType::Float64),
        ]
    );
    assert_eq!(
        result.rows,
        vec![
            vec![
                Value::Int64(14),
                Value::Int64(24),
                Value::Int64(-2),
                Value::Int64(5),
                Value::Float64(10.5),
            ],
            vec![
                Value::Int64(14),
                Value::Int64(18),
                Value::Int64(-5),
                Value::Int64(0),
                Value::Float64(5.25),
            ],
            vec![
                Value::Int64(11),
                Value::Int64(14),
                Value::Int64(-4),
                Value::Int64(0),
                Value::Float64(3.25),
            ],
        ]
    );
}

#[test]
fn aggregate_arguments_and_grouped_projections_accept_expressions() {
    let mut database = sales_database();
    let aggregate = query(
        &mut database,
        "SELECT region,
                SUM(price * quantity) AS revenue,
                AVG(price + adjustment) AS adjusted_mean,
                MAX(price - quantity) AS largest_margin,
                COUNT(price + quantity) AS derived_count
         FROM sales
         GROUP BY region
         ORDER BY revenue DESC;",
    );

    assert_eq!(
        aggregate
            .columns
            .iter()
            .map(|column| column.data_type)
            .collect::<Vec<_>>(),
        vec![
            DataType::String,
            DataType::Int64,
            DataType::Float64,
            DataType::Int64,
            DataType::Int64,
        ]
    );
    assert_eq!(
        aggregate.rows,
        vec![
            vec![
                Value::String("west".to_owned()),
                Value::Int64(32),
                Value::Float64(6.875),
                Value::Int64(8),
                Value::Int64(2),
            ],
            vec![
                Value::String("east".to_owned()),
                Value::Int64(20),
                Value::Float64(5.25),
                Value::Int64(-1),
                Value::Int64(1),
            ],
        ]
    );

    let grouped_projection = query(
        &mut database,
        "SELECT price * 2 AS doubled, COUNT(*) AS rows
         FROM sales GROUP BY price ORDER BY doubled;",
    );
    assert_eq!(
        grouped_projection.rows,
        vec![
            vec![Value::Int64(6), Value::Int64(1)],
            vec![Value::Int64(8), Value::Int64(1)],
            vec![Value::Int64(20), Value::Int64(1)],
        ]
    );
}

#[test]
fn both_comparison_operands_can_be_scalar_expressions() {
    let mut database = sales_database();
    let result = query(
        &mut database,
        "SELECT region, price FROM sales
         WHERE (price + 1) * 2 > quantity + adjustment
           AND price - quantity != 8.0
         ORDER BY price;",
    );

    assert_eq!(
        result.rows,
        vec![
            vec![Value::String("west".to_owned()), Value::Int64(3)],
            vec![Value::String("east".to_owned()), Value::Int64(4)],
        ]
    );
}

#[test]
fn arithmetic_failures_are_reported_from_every_numeric_path() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE boundaries (maximum Int64, minimum Int64, zero Int64, huge Float64);
             INSERT INTO boundaries VALUES
                (9223372036854775807, -9223372036854775808, 0, 1e308);",
        )
        .expect("setup succeeds");

    for sql in [
        "SELECT maximum + 1 FROM boundaries",
        "SELECT minimum - 1 FROM boundaries",
        "SELECT maximum * 2 FROM boundaries",
        "SELECT -minimum FROM boundaries",
        "SELECT minimum / -1 FROM boundaries",
        "SELECT SUM(maximum + 1) FROM boundaries",
        "SELECT maximum FROM boundaries WHERE maximum + 1 > 0",
    ] {
        assert!(
            matches!(database.execute(sql), Err(Error::NumericOverflow(_))),
            "expected overflow for {sql}"
        );
    }

    for sql in [
        "SELECT maximum / zero FROM boundaries",
        "SELECT maximum / 0.0 FROM boundaries",
        "SELECT SUM(maximum / zero) FROM boundaries",
        "SELECT maximum FROM boundaries WHERE maximum / zero > 0",
    ] {
        assert!(
            matches!(database.execute(sql), Err(Error::InvalidQuery(message)) if message.contains("division by zero")),
            "expected division-by-zero error for {sql}"
        );
    }

    assert!(matches!(
        database.execute("SELECT huge * huge FROM boundaries"),
        Err(Error::InvalidQuery(message)) if message.contains("non-finite result")
    ));
}

#[test]
fn non_numeric_arithmetic_is_rejected_during_type_inference() {
    let mut database = sales_database();
    for sql in [
        "SELECT region + 1 FROM sales",
        "SELECT -region FROM sales",
        "SELECT SUM(region + 1) FROM sales",
        "SELECT price FROM sales WHERE region * 2 = 1",
    ] {
        assert!(
            matches!(database.execute(sql), Err(Error::TypeMismatch { expected, .. }) if expected == "Int64 or Float64"),
            "expected numeric type error for {sql}"
        );
    }
}

#[test]
fn ordered_limits_project_only_retained_rows_and_groups() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE ranked (id Int64, denominator Int64, label String);
             INSERT INTO ranked VALUES
                (1, 2, 'retained'),
                (2, 0, 'discarded');",
        )
        .expect("setup succeeds");

    let rows = query(
        &mut database,
        "SELECT id, 10 / denominator AS ratio, label
         FROM ranked ORDER BY id LIMIT 1;",
    );
    assert_eq!(
        rows.rows,
        vec![vec![
            Value::Int64(1),
            Value::Int64(5),
            Value::String("retained".to_owned()),
        ]]
    );

    let groups = query(
        &mut database,
        "SELECT id, 10 / denominator AS ratio, COUNT(*) AS rows
         FROM ranked
         GROUP BY id, denominator
         ORDER BY id
         LIMIT 1;",
    );
    assert_eq!(
        groups.rows,
        vec![vec![Value::Int64(1), Value::Int64(5), Value::Int64(1)]]
    );

    assert!(matches!(
        database.execute(
            "SELECT id, 10 / denominator AS ratio
             FROM ranked ORDER BY ratio LIMIT 1;"
        ),
        Err(Error::InvalidQuery(message)) if message.contains("division by zero")
    ));
}

#[test]
fn scalar_expression_depth_and_node_budgets_are_enforced() {
    let nested = format!(
        "SELECT {}1{} FROM missing",
        "(".repeat(50_000),
        ")".repeat(50_000)
    );
    let nested_error = rusthouse::sql::parse(&nested).expect_err("depth limit rejects query");
    assert!(matches!(
        nested_error,
        Error::Sql { message, .. }
            if message.contains("scalar expression nesting exceeds limit of 64")
    ));

    let unary = format!("SELECT {}1 FROM missing", "- ".repeat(50_000));
    let unary_error = rusthouse::sql::parse(&unary).expect_err("unary depth limit rejects query");
    assert!(matches!(
        unary_error,
        Error::Sql { message, .. }
            if message.contains("scalar expression nesting exceeds limit of 64")
    ));

    let mut balanced = "1".to_owned();
    for _ in 0..8 {
        balanced = format!("({balanced} + {balanced})");
    }
    let nodes = format!("SELECT {balanced} FROM missing");
    let node_error = rusthouse::sql::parse(&nodes).expect_err("node limit rejects query");
    assert!(matches!(
        node_error,
        Error::Sql { message, .. }
            if message.contains("scalar expression is too complex; maximum 256 nodes")
    ));
}
