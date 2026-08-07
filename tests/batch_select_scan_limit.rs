use rusthouse::batch::engine::{Database, QueryResultLimits, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::value::Value;

fn database_with_scan_limit(max_scan_rows: usize) -> Database {
    let mut database = Database::with_query_result_limits(QueryResultLimits {
        max_scan_rows,
        ..QueryResultLimits::default()
    });
    database
        .execute(
            "CREATE TABLE source (n Int64, included Bool); \
             CREATE TABLE boundary (n Int64, included Bool); \
             CREATE TABLE oversized (n Int64, included Bool); \
             INSERT INTO source VALUES (1, false), (2, false), (3, true); \
             INSERT INTO boundary VALUES (1, true), (2, false); \
             INSERT INTO oversized VALUES (3, false), (4, false), (5, false);",
        )
        .expect("setup is not subject to SELECT scan limits");
    database
}

fn scan_limit_error(actual: usize, max: usize) -> Error {
    Error::ResourceLimitExceeded {
        resource: "SELECT scanned rows",
        actual,
        max,
    }
}

#[test]
fn regular_select_checks_the_full_source_before_where_or_limit() {
    let mut database = database_with_scan_limit(2);

    assert_eq!(
        database
            .execute("SELECT n FROM source WHERE included = true LIMIT 1")
            .expect_err("a selective predicate and LIMIT cannot bypass the source bound"),
        scan_limit_error(3, 2)
    );
    assert_eq!(
        database
            .execute("SELECT n FROM source LIMIT 0")
            .expect_err("LIMIT 0 cannot bypass the source bound"),
        scan_limit_error(3, 2)
    );

    let results = database
        .execute("SELECT n FROM boundary WHERE included = true LIMIT 1")
        .expect("the exact scan boundary is accepted");
    let [StatementResult::Query(result)] = results.as_slice() else {
        panic!("expected one query result");
    };
    assert_eq!(result.rows, [vec![Value::Int64(1)]]);
}

#[test]
fn distinct_and_each_union_operand_enforce_the_source_bound() {
    let mut database = database_with_scan_limit(2);

    assert_eq!(
        database
            .execute("SELECT DISTINCT n FROM source WHERE included = true LIMIT 1")
            .expect_err("DISTINCT cannot bypass the source bound"),
        scan_limit_error(3, 2)
    );
    assert_eq!(
        database
            .execute(
                "SELECT n FROM oversized WHERE included = true LIMIT 0 \
                 UNION ALL SELECT n FROM boundary LIMIT 0",
            )
            .expect_err("the left UNION operand is bounded"),
        scan_limit_error(3, 2)
    );
    assert_eq!(
        database
            .execute(
                "SELECT n FROM boundary LIMIT 0 \
                 UNION ALL SELECT n FROM oversized WHERE included = true LIMIT 0",
            )
            .expect_err("the right UNION operand is bounded"),
        scan_limit_error(3, 2)
    );

    let results = database
        .execute(
            "SELECT DISTINCT n FROM boundary LIMIT 1 \
             UNION ALL SELECT n FROM boundary WHERE included = false LIMIT 1",
        )
        .expect("each operand at the exact boundary is accepted");
    let [StatementResult::Query(result)] = results.as_slice() else {
        panic!("expected one query result");
    };
    assert_eq!(result.rows, [vec![Value::Int64(1)], vec![Value::Int64(2)]]);
}
