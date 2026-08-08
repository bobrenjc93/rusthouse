use rusthouse::batch::engine::{Database, QueryResult, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{SelectItem, Statement, parse};
use rusthouse::batch::value::Value;

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    match results.into_iter().last().expect("one result") {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

#[test]
fn parses_limit_offset_for_regular_and_physical_column_distinct_selects() {
    let statements = parse(
        "SELECT reading - 1 AS adjusted FROM samples \
         WHERE reading >= 0 ORDER BY adjusted DESC LIMIT 2 OFFSET 3",
    )
    .expect("LIMIT OFFSET projection parses");
    let [Statement::Select(select)] = statements.as_slice() else {
        panic!("expected regular SELECT");
    };
    assert!(matches!(
        select.items.as_slice(),
        [SelectItem::Int64Subtract { .. }]
    ));
    assert!(select.predicate.is_some());
    assert_eq!(select.limit, Some(2));
    assert_eq!(select.offset, Some(3));

    let plain_limit = parse("SELECT reading FROM samples LIMIT 4").expect("plain LIMIT parses");
    let [Statement::Select(select)] = plain_limit.as_slice() else {
        panic!("expected regular SELECT");
    };
    assert_eq!(select.limit, Some(4));
    assert_eq!(select.offset, None);

    let distinct = parse("SELECT DISTINCT reading FROM samples ORDER BY reading LIMIT 2 OFFSET 1")
        .expect("physical-column DISTINCT pagination parses");
    let [Statement::Select(select)] = distinct.as_slice() else {
        panic!("expected DISTINCT SELECT");
    };
    assert!(select.distinct);
    assert_eq!(select.limit, Some(2));
    assert_eq!(select.offset, Some(1));
}

#[test]
fn rejects_malformed_offsets_and_unsupported_select_shapes() {
    for sql in [
        "SELECT n FROM samples OFFSET 1",
        "SELECT n FROM samples LIMIT 1 OFFSET",
        "SELECT n FROM samples LIMIT 1 OFFSET -1",
        "SELECT n FROM samples LIMIT 1 OFFSET 1.5",
        "SELECT n FROM samples LIMIT 1 OFFSET many",
        "SELECT DISTINCT n FROM samples OFFSET 1",
        "SELECT DISTINCT n FROM samples LIMIT 1 OFFSET",
        "SELECT DISTINCT n FROM samples LIMIT 1 OFFSET -1",
        "SELECT DISTINCT n FROM samples LIMIT 1 OFFSET 1.5",
        "SELECT DISTINCT n FROM samples LIMIT 1 OFFSET many",
        "SELECT COUNT(*) FROM samples LIMIT 1 OFFSET 1",
        "SELECT n, COUNT(*) FROM samples GROUP BY n LIMIT 1 OFFSET 1",
        "SELECT n, ROW_NUMBER() OVER () FROM samples LIMIT 1 OFFSET 1",
        "SELECT 1 LIMIT 1 OFFSET 1",
        "SELECT * FROM samples CROSS JOIN other LIMIT 1 OFFSET 1",
    ] {
        assert!(parse(sql).is_err(), "{sql:?} must be rejected");
    }
}

#[test]
fn filters_and_orders_before_applying_zero_and_nonzero_offsets() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (reading Int64, included Bool); \
             INSERT INTO samples VALUES \
                 (30, true), (10, true), (20, false), (50, true), (40, true);",
        )
        .expect("fixture succeeds");

    assert_eq!(
        query(
            &mut database,
            "SELECT reading FROM samples WHERE included = true LIMIT 2 OFFSET 1",
        )
        .rows,
        [vec![Value::Int64(10)], vec![Value::Int64(50)]]
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT reading FROM samples WHERE included = true \
             ORDER BY reading ASC LIMIT 2 OFFSET 1",
        )
        .rows,
        [vec![Value::Int64(30)], vec![Value::Int64(40)]]
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT reading FROM samples ORDER BY reading DESC LIMIT 2 OFFSET 0",
        )
        .rows,
        [vec![Value::Int64(50)], vec![Value::Int64(40)]]
    );
}

#[test]
fn zero_count_and_beyond_end_offsets_return_no_rows() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (reading Int64); \
             INSERT INTO samples VALUES (3), (1), (2);",
        )
        .expect("fixture succeeds");

    for sql in [
        "SELECT reading FROM samples LIMIT 0 OFFSET 0",
        "SELECT reading FROM samples LIMIT 0 OFFSET 2",
        "SELECT reading FROM samples ORDER BY reading LIMIT 2 OFFSET 3",
        "SELECT reading FROM samples LIMIT 1 OFFSET 9",
    ] {
        assert!(query(&mut database, sql).rows.is_empty(), "{sql}");
    }
}

#[test]
fn rejects_an_overflowing_count_plus_offset_bound() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE samples (reading Int64); INSERT INTO samples VALUES (1);")
        .expect("fixture succeeds");

    let sql = format!(
        "SELECT reading FROM samples ORDER BY reading LIMIT {} OFFSET 1",
        usize::MAX
    );
    assert_eq!(
        database.execute(&sql),
        Err(Error::NumericOverflow(
            "LIMIT + OFFSET selection bound".to_owned()
        ))
    );
}

#[test]
fn skipped_rows_do_not_evaluate_checked_scalar_projections() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE bounds (reading Int64); \
             INSERT INTO bounds VALUES (-9223372036854775808), (0), (9223372036854775807);",
        )
        .expect("fixture succeeds");

    assert_eq!(
        query(
            &mut database,
            "SELECT reading - 1 AS adjusted FROM bounds \
             ORDER BY adjusted ASC LIMIT 1 OFFSET 1",
        )
        .rows,
        [vec![Value::Int64(-1)]]
    );
    assert_eq!(
        database.execute(
            "SELECT reading - 1 AS adjusted FROM bounds \
             ORDER BY adjusted ASC LIMIT 1 OFFSET 0",
        ),
        Err(Error::NumericOverflow("Int64 subtraction".to_owned()))
    );
}
