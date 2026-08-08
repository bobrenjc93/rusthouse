use rusthouse::batch::engine::{Database, QueryResult, QueryResultLimits, StatementResult};
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
fn parses_documented_limit_offset_for_regular_distinct_and_grouped_selects() {
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

    for sql in [
        "SELECT COUNT(*) AS rows FROM samples LIMIT 1 OFFSET 1",
        "SELECT n, COUNT(*) AS rows FROM samples GROUP BY n LIMIT 2 OFFSET 3",
        "SELECT n, SUM(n) AS total FROM samples GROUP BY n \
         HAVING total > 0 ORDER BY total DESC LIMIT 4 OFFSET 5",
    ] {
        let statements = parse(sql).expect("aggregate LIMIT OFFSET syntax parses");
        let [Statement::Select(select)] = statements.as_slice() else {
            panic!("expected aggregate SELECT");
        };
        assert!(
            select
                .items
                .iter()
                .any(|item| matches!(item, SelectItem::Aggregate { .. }))
        );
        assert!(select.limit.is_some());
        assert!(select.offset.is_some());
    }
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
        "SELECT n, ROW_NUMBER() OVER () FROM samples LIMIT 1 OFFSET 1",
        "SELECT 1 LIMIT 1 OFFSET 1",
        "SELECT * FROM samples CROSS JOIN other LIMIT 1 OFFSET 1",
    ] {
        assert!(parse(sql).is_err(), "{sql:?} must be rejected");
    }
}

#[test]
fn pages_unordered_and_ordered_groups_after_where_and_having() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (kind String, amount Int64, included Bool); \
             INSERT INTO events VALUES \
                 ('alpha', 1, true), \
                 ('beta', 5, true), ('beta', 7, true), \
                 ('gamma', 2, true), ('gamma', 3, true), ('gamma', 4, true), \
                 ('ignored', 100, false);",
        )
        .expect("fixture succeeds");

    assert_eq!(
        query(
            &mut database,
            "SELECT kind, COUNT(*) AS n FROM events WHERE included = true \
             GROUP BY kind HAVING n >= 2 LIMIT 1 OFFSET 1",
        )
        .rows,
        [vec![Value::String("gamma".to_owned()), Value::Int64(3)]]
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT kind, COUNT(*) AS n FROM events WHERE included = true \
             GROUP BY kind HAVING n >= 2 \
             ORDER BY n DESC, kind ASC LIMIT 1 OFFSET 1",
        )
        .rows,
        [vec![Value::String("beta".to_owned()), Value::Int64(2)]]
    );
}

#[test]
fn pages_global_aggregates_and_empty_aggregate_results() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (amount Int64, included Bool); \
             INSERT INTO events VALUES (2, true), (3, false), (5, true);",
        )
        .expect("fixture succeeds");

    assert_eq!(
        query(
            &mut database,
            "SELECT COUNT(*) AS n, SUM(amount) AS total FROM events \
             WHERE included = true HAVING n > 0 LIMIT 1 OFFSET 0",
        )
        .rows,
        [vec![Value::Int64(2), Value::Int64(7)]]
    );
    assert!(
        query(
            &mut database,
            "SELECT COUNT(*) AS n FROM events LIMIT 1 OFFSET 1",
        )
        .rows
        .is_empty()
    );

    database
        .execute("CREATE TABLE empty (amount Int64);")
        .expect("empty fixture succeeds");
    assert_eq!(
        query(
            &mut database,
            "SELECT COUNT(*) AS n FROM empty LIMIT 1 OFFSET 0",
        )
        .rows,
        [vec![Value::Int64(0)]]
    );
    assert!(
        query(
            &mut database,
            "SELECT SUM(amount) AS total FROM empty LIMIT 1 OFFSET 1",
        )
        .rows
        .is_empty()
    );
}

#[test]
fn grouped_zero_count_and_excessive_offsets_return_no_rows() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE samples (reading Int64); \
             INSERT INTO samples VALUES (3), (1), (2);",
        )
        .expect("fixture succeeds");

    for sql in [
        "SELECT reading, COUNT(*) FROM samples GROUP BY reading LIMIT 0 OFFSET 0",
        "SELECT reading, COUNT(*) FROM samples GROUP BY reading LIMIT 0 OFFSET 2",
        "SELECT reading, COUNT(*) FROM samples GROUP BY reading \
         ORDER BY reading LIMIT 2 OFFSET 3",
        "SELECT reading, COUNT(*) FROM samples GROUP BY reading LIMIT 1 OFFSET 9",
    ] {
        assert!(query(&mut database, sql).rows.is_empty(), "{sql}");
    }
}

#[test]
fn grouped_offset_does_not_reduce_group_working_limits() {
    let mut database = Database::with_query_result_limits(QueryResultLimits {
        max_groups: 2,
        ..QueryResultLimits::default()
    });
    database
        .execute(
            "CREATE TABLE samples (reading Int64); \
             INSERT INTO samples VALUES (1), (2), (3);",
        )
        .expect("fixture succeeds");

    assert_eq!(
        database.execute(
            "SELECT reading, COUNT(*) FROM samples \
             GROUP BY reading LIMIT 1 OFFSET 1",
        ),
        Err(Error::ResourceLimitExceeded {
            resource: "SELECT groups",
            actual: 3,
            max: 2,
        })
    );
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
fn rejects_overflowing_grouped_and_global_aggregate_selection_bounds() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE samples (reading Int64); INSERT INTO samples VALUES (1);")
        .expect("fixture succeeds");

    for sql in [
        format!("SELECT COUNT(*) FROM samples LIMIT {} OFFSET 1", usize::MAX),
        format!(
            "SELECT reading, COUNT(*) FROM samples GROUP BY reading \
             ORDER BY reading LIMIT {} OFFSET 1",
            usize::MAX
        ),
    ] {
        assert_eq!(
            database.execute(&sql),
            Err(Error::NumericOverflow(
                "LIMIT + OFFSET selection bound".to_owned()
            )),
            "{sql}"
        );
    }
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
