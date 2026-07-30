use rusthouse::format::{OutputFormat, render};
use rusthouse::{DataType, Database, Decimal128, Error, QueryResult, StatementResult, Value};

fn decimal(coefficient: i128, precision: u8, scale: u8) -> Value {
    Value::Decimal128(
        Decimal128::new(coefficient, precision, scale).expect("valid test Decimal128 value"),
    )
}

fn last_query(results: Vec<StatementResult>) -> QueryResult {
    match results.into_iter().last().expect("statement result") {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

fn query(database: &mut Database, sql: &str) -> QueryResult {
    last_query(database.execute(sql).expect("query succeeds"))
}

#[test]
fn decimal_filters_groups_orders_and_aggregates_exactly() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE ledger (label String, amount Decimal128(8, 2));
             INSERT INTO ledger VALUES
                ('negative', -1.235),
                ('first', 3.335),
                ('same', 3.336);",
        )
        .expect("setup succeeds");

    let filtered = query(
        &mut database,
        "SELECT label, amount FROM ledger
         WHERE amount >= 3.335 ORDER BY amount, label;",
    );
    assert_eq!(
        filtered.rows,
        vec![
            vec![Value::String("first".to_owned()), decimal(334, 8, 2)],
            vec![Value::String("same".to_owned()), decimal(334, 8, 2)],
        ]
    );

    let grouped = query(
        &mut database,
        "SELECT amount, COUNT(*) AS rows FROM ledger
         GROUP BY amount ORDER BY amount;",
    );
    assert_eq!(
        grouped.rows,
        vec![
            vec![decimal(-124, 8, 2), Value::Int64(1)],
            vec![decimal(334, 8, 2), Value::Int64(2)],
        ]
    );

    let aggregates = query(
        &mut database,
        "SELECT SUM(amount) AS total, MIN(amount) AS low,
                MAX(amount) AS high, AVG(amount) AS mean
         FROM ledger;",
    );
    assert_eq!(
        aggregates
            .columns
            .iter()
            .map(|column| column.data_type)
            .collect::<Vec<_>>(),
        vec![
            DataType::Decimal128 {
                precision: 8,
                scale: 2,
            };
            4
        ]
    );
    assert_eq!(
        aggregates.rows,
        vec![vec![
            decimal(544, 8, 2),
            decimal(-124, 8, 2),
            decimal(334, 8, 2),
            decimal(181, 8, 2),
        ]]
    );

    let mut comparisons = Database::new();
    comparisons
        .execute(
            "CREATE TABLE comparisons (
                label String,
                left_value Decimal128(6, 2),
                right_value Decimal128(7, 3),
                whole Int64
             );
             INSERT INTO comparisons VALUES
                ('scaled', 1.20, 1.200, 0),
                ('integer', 2.00, 2.001, 2);",
        )
        .expect("comparison setup succeeds");
    let result = query(
        &mut comparisons,
        "SELECT label FROM comparisons
         WHERE left_value = right_value OR left_value = whole
         ORDER BY label;",
    );
    assert_eq!(
        result.rows,
        vec![
            vec![Value::String("integer".to_owned())],
            vec![Value::String("scaled".to_owned())],
        ]
    );
}

#[test]
fn decimal_rounding_is_half_away_from_zero() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE rounding (amount Decimal128(5, 2));
             INSERT INTO rounding VALUES
                (1.234), (1.235), (-1.234), (-1.235), (0.005), (-0.005);",
        )
        .expect("rounded values fit");

    let result = query(&mut database, "SELECT amount FROM rounding;");
    assert_eq!(
        result.rows,
        vec![
            vec![decimal(123, 5, 2)],
            vec![decimal(124, 5, 2)],
            vec![decimal(-123, 5, 2)],
            vec![decimal(-124, 5, 2)],
            vec![decimal(1, 5, 2)],
            vec![decimal(-1, 5, 2)],
        ]
    );

    let mut averages = Database::new();
    averages
        .execute(
            "CREATE TABLE samples (negative Bool, amount Decimal128(4, 2));
             INSERT INTO samples VALUES
                (false, 0.00), (false, 0.01),
                (true, 0.00), (true, -0.01);",
        )
        .expect("setup succeeds");
    let result = query(
        &mut averages,
        "SELECT negative, AVG(amount) AS mean
         FROM samples GROUP BY negative ORDER BY negative;",
    );
    assert_eq!(
        result.rows,
        vec![
            vec![Value::Bool(false), decimal(1, 4, 2)],
            vec![Value::Bool(true), decimal(-1, 4, 2)],
        ]
    );
}

#[test]
fn precision_boundaries_and_declarations_are_checked() {
    let mut database = Database::new();
    let maximum = "99999999999999999999999999999999999999";
    database
        .execute(&format!(
            "CREATE TABLE boundary (amount Decimal128(38, 0));
             INSERT INTO boundary VALUES ({maximum}), (-{maximum});"
        ))
        .expect("38-digit boundary values fit");
    let result = query(
        &mut database,
        "SELECT amount FROM boundary ORDER BY amount;",
    );
    assert_eq!(
        result.rows,
        vec![
            vec![decimal(
                -99_999_999_999_999_999_999_999_999_999_999_999_999,
                38,
                0
            )],
            vec![decimal(
                99_999_999_999_999_999_999_999_999_999_999_999_999,
                38,
                0
            )],
        ]
    );

    for declaration in ["Decimal128(0, 0)", "Decimal128(39, 0)", "Decimal128(4, 5)"] {
        let error = Database::new()
            .execute(&format!("CREATE TABLE invalid (amount {declaration});"))
            .expect_err("invalid Decimal128 declaration");
        assert!(matches!(error, Error::Sql { .. }));
    }
}

#[test]
fn decimal_insert_failure_is_atomic() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE atomic_values (amount Decimal128(5, 2));")
        .expect("create succeeds");

    let error = database
        .execute("INSERT INTO atomic_values VALUES (1.00), (999.995);")
        .expect_err("rounding the second value exceeds the precision");
    assert!(matches!(error, Error::NumericOverflow(operation) if operation.contains("literal")));

    let result = query(&mut database, "SELECT COUNT(*) AS rows FROM atomic_values;");
    assert_eq!(result.rows, vec![vec![Value::Int64(0)]]);
}

#[test]
fn decimal_aggregate_overflow_is_reported() {
    let mut sum_database = Database::new();
    sum_database
        .execute(
            "CREATE TABLE totals (amount Decimal128(3, 0));
             INSERT INTO totals VALUES (600), (400);",
        )
        .expect("inputs fit individually");
    let error = sum_database
        .execute("SELECT SUM(amount) FROM totals;")
        .expect_err("sum exceeds declared precision");
    assert!(
        matches!(error, Error::NumericOverflow(operation) if operation == "SUM(Decimal128(3, 0))")
    );

    let mut cancellation_database = Database::new();
    cancellation_database
        .execute(
            "CREATE TABLE cancellation (amount Decimal128(3, 0));
             INSERT INTO cancellation VALUES (600), (400), (-100);",
        )
        .expect("inputs fit individually");
    let cancellation = query(
        &mut cancellation_database,
        "SELECT SUM(amount) AS total FROM cancellation;",
    );
    assert_eq!(cancellation.rows, vec![vec![decimal(900, 3, 0)]]);

    let mut avg_database = Database::new();
    let maximum = "99999999999999999999999999999999999999";
    avg_database
        .execute(&format!(
            "CREATE TABLE averages (amount Decimal128(38, 0));
             INSERT INTO averages VALUES ({maximum}), ({maximum});"
        ))
        .expect("inputs fit individually");
    let error = avg_database
        .execute("SELECT AVG(amount) FROM averages;")
        .expect_err("average accumulation exceeds i128");
    assert!(
        matches!(error, Error::NumericOverflow(operation) if operation.contains("AVG(Decimal128(38, 0)) sum"))
    );
}

#[test]
fn every_output_format_preserves_decimal_text() {
    let mut database = Database::new();
    let result = query(
        &mut database,
        "CREATE TABLE output (amount Decimal128(38, 4));
         INSERT INTO output VALUES (123456789012345678901234567890.1200);
         SELECT amount FROM output;",
    );

    assert!(render(&result, OutputFormat::Table).contains("123456789012345678901234567890.1200"));
    assert_eq!(
        render(&result, OutputFormat::Csv),
        "amount\n123456789012345678901234567890.1200\n"
    );
    assert_eq!(
        render(&result, OutputFormat::Json),
        r#"{"columns":[{"name":"amount","type":"Decimal128(38, 4)"}],"rows":[[123456789012345678901234567890.1200]]}"#
    );
}
