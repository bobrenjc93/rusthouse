use rusthouse::SharedDatabase;
use rusthouse::batch::engine::{
    Database, ESTIMATED_GROUP_KEY_CELL_BYTES, QueryResult, QueryResultLimits, ResultColumn,
    StatementResult,
};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{Statement, parse};
use rusthouse::batch::value::{DataType, Value};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    match results.into_iter().last().expect("one result") {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

fn database_with_limits(limits: QueryResultLimits) -> Database {
    let mut database = Database::with_query_result_limits(limits);
    database
        .execute(
            "CREATE TABLE left_rows (i Int64, f Float64, b Bool, s String); \
             CREATE TABLE right_rows (i Int64, f Float64, b Bool, s String); \
             INSERT INTO left_rows VALUES (1, 1.5, true, 'a'), (1, 1.5, true, 'a'); \
             INSERT INTO right_rows VALUES \
                 (1, 1.5, true, 'a'), (2, 2.5, false, 'b'), (2, 2.5, false, 'b');",
        )
        .expect("setup succeeds");
    database
}

#[test]
fn parses_exact_union_distinct_between_two_complete_selects() {
    let statements = parse(
        "SELECT DISTINCT kind FROM left_events LIMIT 2 \
         UNION DISTINCT \
         SELECT kind FROM right_events WHERE active = true ORDER BY kind LIMIT 3;",
    )
    .expect("two SELECT operands are valid");

    let [Statement::UnionDistinct { left, right }] = statements.as_slice() else {
        panic!("expected one UNION DISTINCT statement");
    };
    assert!(left.distinct);
    assert_eq!(left.table, "left_events");
    assert_eq!(left.limit, Some(2));
    assert_eq!(right.table, "right_events");
    assert!(right.predicate.is_some());
    assert_eq!(right.order_by.len(), 1);
    assert_eq!(right.limit, Some(3));
}

#[test]
fn pages_physical_column_distinct_union_operands_before_combining_them() {
    let statements = parse(
        "SELECT DISTINCT n FROM older ORDER BY n LIMIT 2 OFFSET 1 \
         UNION DISTINCT \
         SELECT DISTINCT n FROM newer LIMIT 2 OFFSET 1",
    )
    .expect("paginated DISTINCT operands parse");
    let [Statement::UnionDistinct { left, right }] = statements.as_slice() else {
        panic!("expected one UNION DISTINCT statement");
    };
    assert!(left.distinct);
    assert_eq!((left.limit, left.offset), (Some(2), Some(1)));
    assert!(right.distinct);
    assert_eq!((right.limit, right.offset), (Some(2), Some(1)));

    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE older (n Int64); \
             CREATE TABLE newer (n Int64); \
             INSERT INTO older VALUES (3), (1), (3), (2); \
             INSERT INTO newer VALUES (3), (4), (4), (5);",
        )
        .expect("setup succeeds");

    assert_eq!(
        query(
            &mut database,
            "SELECT DISTINCT n FROM older ORDER BY n LIMIT 2 OFFSET 1 \
             UNION DISTINCT \
             SELECT DISTINCT n FROM newer LIMIT 2 OFFSET 1",
        )
        .rows,
        [
            vec![Value::Int64(2)],
            vec![Value::Int64(3)],
            vec![Value::Int64(4)],
            vec![Value::Int64(5)],
        ]
    );
}

#[test]
fn rejects_plain_incomplete_nested_and_outer_union_distinct_syntax() {
    for sql in [
        "SELECT n FROM l UNION SELECT n FROM r",
        "SELECT n FROM l UNION DISTINCT",
        "SELECT n FROM l UNION DISTINCT SHOW TABLES",
        "SELECT n FROM l UNION DISTINCT (SELECT n FROM r)",
        "SELECT n FROM l UNION DISTINCT SELECT n FROM r UNION DISTINCT SELECT n FROM x",
        "SELECT n FROM l UNION DISTINCT SELECT n FROM r UNION ALL SELECT n FROM x",
        "SELECT n FROM l UNION ALL SELECT n FROM r UNION DISTINCT SELECT n FROM x",
        "SELECT n FROM l UNION DISTINCT SELECT n FROM r FORMAT CSV",
    ] {
        assert!(parse(sql).is_err(), "{sql:?} must be rejected");
    }
}

#[test]
fn retains_first_complete_typed_rows_across_and_within_operands() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE older (i Int64, f Float64, b Bool, s String); \
             CREATE TABLE newer (i Int64, f Float64, b Bool, s String); \
             INSERT INTO older VALUES \
                 (1, 1.5, true, 'first'), (1, 1.5, true, 'first'), \
                 (2, -0.0, false, 'second'); \
             INSERT INTO newer VALUES \
                 (2, 0.0, false, 'second'), (3, 3.5, true, 'third'), \
                 (3, 3.5, true, 'third'), (1, 1.5, true, 'variant');",
        )
        .expect("setup succeeds");

    let result = query(
        &mut database,
        "SELECT i AS integer, f AS float, b AS boolean, s AS string FROM older \
         UNION DISTINCT \
         SELECT i AS ignored_i, f AS ignored_f, b AS ignored_b, s AS ignored_s FROM newer",
    );

    assert_eq!(
        result.columns,
        [
            ResultColumn {
                name: "integer".to_owned(),
                data_type: DataType::Int64,
            },
            ResultColumn {
                name: "float".to_owned(),
                data_type: DataType::Float64,
            },
            ResultColumn {
                name: "boolean".to_owned(),
                data_type: DataType::Bool,
            },
            ResultColumn {
                name: "string".to_owned(),
                data_type: DataType::String,
            },
        ]
    );
    assert_eq!(
        result.rows,
        [
            vec![
                Value::Int64(1),
                Value::Float64(1.5),
                Value::Bool(true),
                Value::String("first".to_owned()),
            ],
            vec![
                Value::Int64(2),
                Value::Float64(-0.0),
                Value::Bool(false),
                Value::String("second".to_owned()),
            ],
            vec![
                Value::Int64(3),
                Value::Float64(3.5),
                Value::Bool(true),
                Value::String("third".to_owned()),
            ],
            vec![
                Value::Int64(1),
                Value::Float64(1.5),
                Value::Bool(true),
                Value::String("variant".to_owned()),
            ],
        ]
    );
}

#[test]
fn deduplicates_typed_nulls_from_empty_input_aggregates() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE empty_left (i Int64, f Float64, b Bool, s String); \
             CREATE TABLE empty_right (i Int64, f Float64, b Bool, s String);",
        )
        .expect("setup succeeds");

    let result = query(
        &mut database,
        "SELECT MIN(i), MIN(f), MIN(b), MIN(s) FROM empty_left \
         UNION DISTINCT \
         SELECT MIN(i), MIN(f), MIN(b), MIN(s) FROM empty_right",
    );

    assert_eq!(
        result.rows,
        [vec![
            Value::Null(DataType::Int64),
            Value::Null(DataType::Float64),
            Value::Null(DataType::Bool),
            Value::Null(DataType::String),
        ]]
    );
}

#[test]
fn rejects_column_count_and_type_mismatches() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE left_rows (id Int64, label String); \
             CREATE TABLE right_rows (id Int64, label String);",
        )
        .expect("setup succeeds");

    assert_eq!(
        database
            .execute("SELECT id FROM left_rows UNION DISTINCT SELECT id, label FROM right_rows")
            .expect_err("column counts must match"),
        Error::UnionDistinctColumnCountMismatch { left: 1, right: 2 }
    );
    assert_eq!(
        database
            .execute("SELECT id FROM left_rows UNION DISTINCT SELECT label FROM right_rows")
            .expect_err("column types must match"),
        Error::TypeMismatch {
            context: "UNION DISTINCT column 1".to_owned(),
            expected: "Int64".to_owned(),
            actual: "String".to_owned(),
        }
    );
}

#[test]
fn enforces_raw_result_and_deduplication_limits_at_exact_boundaries() {
    let raw_rows = 5;
    let column_count = 4;
    let raw_string_bytes = 5;
    let result_bytes = column_count * std::mem::size_of::<ResultColumn>()
        + 4
        + raw_rows * std::mem::size_of::<Vec<Value>>()
        + raw_rows * column_count * std::mem::size_of::<Value>()
        + raw_string_bytes;
    let key_cells = 4 + 2 * column_count;
    let key_bytes = key_cells * ESTIMATED_GROUP_KEY_CELL_BYTES;
    let exact_limits = QueryResultLimits {
        max_rows: raw_rows,
        max_values: raw_rows * column_count,
        max_bytes: result_bytes,
        max_groups: 2,
        max_group_key_cells: key_cells,
        max_group_key_bytes: key_bytes,
        ..QueryResultLimits::default()
    };
    let sql = "SELECT i, f, b, s FROM left_rows \
               UNION DISTINCT SELECT i, f, b, s FROM right_rows";

    assert_eq!(
        query(&mut database_with_limits(exact_limits), sql).rows,
        [
            vec![
                Value::Int64(1),
                Value::Float64(1.5),
                Value::Bool(true),
                Value::String("a".to_owned()),
            ],
            vec![
                Value::Int64(2),
                Value::Float64(2.5),
                Value::Bool(false),
                Value::String("b".to_owned()),
            ],
        ]
    );

    for (limits, expected) in [
        (
            QueryResultLimits {
                max_rows: raw_rows - 1,
                ..exact_limits
            },
            Error::ResourceLimitExceeded {
                resource: "SELECT result rows",
                actual: raw_rows,
                max: raw_rows - 1,
            },
        ),
        (
            QueryResultLimits {
                max_values: raw_rows * column_count - 1,
                ..exact_limits
            },
            Error::ResourceLimitExceeded {
                resource: "SELECT result values",
                actual: raw_rows * column_count,
                max: raw_rows * column_count - 1,
            },
        ),
        (
            QueryResultLimits {
                max_bytes: result_bytes - 1,
                ..exact_limits
            },
            Error::ResourceLimitExceeded {
                resource: "SELECT result bytes",
                actual: result_bytes,
                max: result_bytes - 1,
            },
        ),
        (
            QueryResultLimits {
                max_groups: 1,
                ..exact_limits
            },
            Error::ResourceLimitExceeded {
                resource: "SELECT groups",
                actual: 2,
                max: 1,
            },
        ),
        (
            QueryResultLimits {
                max_group_key_cells: key_cells - 1,
                ..exact_limits
            },
            Error::ResourceLimitExceeded {
                resource: "SELECT group key cells",
                actual: key_cells,
                max: key_cells - 1,
            },
        ),
        (
            QueryResultLimits {
                max_group_key_bytes: key_bytes - 1,
                ..exact_limits
            },
            Error::ResourceLimitExceeded {
                resource: "SELECT group key bytes",
                actual: key_bytes,
                max: key_bytes - 1,
            },
        ),
    ] {
        assert_eq!(
            database_with_limits(limits)
                .execute(sql)
                .expect_err("one exact resource bound is exceeded"),
            expected
        );
    }
}

#[test]
fn empty_operands_require_no_deduplication_state() {
    let limits = QueryResultLimits {
        max_rows: 0,
        max_values: 0,
        max_groups: 0,
        max_group_key_cells: 0,
        max_group_key_bytes: 0,
        ..QueryResultLimits::default()
    };
    let mut database = Database::with_query_result_limits(limits);
    database
        .execute(
            "CREATE TABLE empty_left (n Int64); \
             CREATE TABLE empty_right (n Int64);",
        )
        .expect("setup succeeds");

    assert!(
        query(
            &mut database,
            "SELECT n FROM empty_left UNION DISTINCT SELECT n FROM empty_right",
        )
        .rows
        .is_empty()
    );
}

#[test]
fn multi_result_retained_limit_does_not_keep_raw_union_row_capacity() {
    const RAW_ROWS: usize = 10_000;
    const OPERAND_ROWS: usize = RAW_ROWS / 2;
    const RESULT_COUNT: usize = 3;

    let mut setup = String::from(
        "CREATE TABLE left_rows (n Int64); CREATE TABLE right_rows (n Int64); \
         INSERT INTO left_rows VALUES ",
    );
    for row in 0..OPERAND_ROWS {
        if row > 0 {
            setup.push(',');
        }
        setup.push_str("(1)");
    }
    setup.push_str("; INSERT INTO right_rows VALUES ");
    for row in 0..OPERAND_ROWS {
        if row > 0 {
            setup.push(',');
        }
        setup.push_str("(1)");
    }
    setup.push(';');

    let mut database = Database::new();
    database.execute(&setup).expect("setup succeeds");

    let query = "SELECT n FROM left_rows UNION DISTINCT SELECT n FROM right_rows";
    let batch = std::iter::repeat_n(query, RESULT_COUNT)
        .collect::<Vec<_>>()
        .join(";");
    let one_retained_result = std::mem::size_of::<ResultColumn>()
        + "n".len()
        + std::mem::size_of::<Vec<Value>>()
        + std::mem::size_of::<Value>();
    let one_raw_result = std::mem::size_of::<ResultColumn>()
        + "n".len()
        + RAW_ROWS * std::mem::size_of::<Vec<Value>>()
        + RAW_ROWS * std::mem::size_of::<Value>();
    let retained_limit = one_raw_result + (RESULT_COUNT - 1) * one_retained_result;
    let uncompacted_outer_row_bytes = RESULT_COUNT * RAW_ROWS * std::mem::size_of::<Vec<Value>>();
    assert!(uncompacted_outer_row_bytes > retained_limit);

    let results = database
        .execute_with_result_limit(&batch, retained_limit)
        .expect("each raw result fits while compacted results remain retained");
    assert_eq!(results.len(), RESULT_COUNT);

    let mut retained_outer_row_bytes = 0;
    for result in results {
        let StatementResult::Query(result) = result else {
            panic!("every statement returns a query result");
        };
        assert_eq!(result.rows, [vec![Value::Int64(1)]]);
        assert_eq!(
            result.rows.capacity(),
            result.rows.len(),
            "UNION DISTINCT must release raw outer row capacity"
        );
        retained_outer_row_bytes += result.rows.capacity() * std::mem::size_of::<Vec<Value>>();
    }
    assert!(retained_outer_row_bytes <= retained_limit);
}

#[test]
fn shared_database_accepts_union_distinct_as_one_read_only_query() {
    let database = SharedDatabase::default();
    database
        .execute(
            "CREATE TABLE first (n Int64); CREATE TABLE second (n Int64); \
             INSERT INTO first VALUES (1), (1); INSERT INTO second VALUES (1), (2);",
        )
        .expect("setup succeeds");

    let result = database
        .query("SELECT n FROM first UNION DISTINCT SELECT n FROM second")
        .expect("UNION DISTINCT is read-only");
    assert_eq!(result.rows, [vec![Value::Int64(1)], vec![Value::Int64(2)]]);
}
