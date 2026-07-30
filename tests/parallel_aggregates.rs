use std::fmt::Write as _;

use rusthouse::{Database, Error, QueryResult, StatementResult, Value};

const ROW_COUNT: usize = 33_000;
const PARTITION_BOUNDARY: usize = 16 * 1_024;
const THREAD_COUNTS: [usize; 4] = [1, 2, 4, 8];

fn generated_database() -> Database {
    let mut sql = String::with_capacity(ROW_COUNT * 100);
    sql.push_str(
        "CREATE TABLE generated (id Int64, bucket String, unique_key String, value Int64, \
         score Float64, included Bool, safe_sum Int64, overflow_sum Int64, \
         safe_float Float64, overflow_float Float64); \
         INSERT INTO generated VALUES ",
    );
    for id in 0..ROW_COUNT {
        if id > 0 {
            sql.push(',');
        }
        let safe_sum = match id {
            value if value + 1 == PARTITION_BOUNDARY => -i64::MAX,
            value if value == PARTITION_BOUNDARY || value == PARTITION_BOUNDARY + 1 => i64::MAX,
            _ => 0,
        };
        let overflow_sum = match id {
            value if value + 1 == PARTITION_BOUNDARY => i64::MAX,
            PARTITION_BOUNDARY => 1,
            value if value == PARTITION_BOUNDARY + 1 => -1,
            _ => 0,
        };
        let safe_float = match id {
            value if value + 1 == PARTITION_BOUNDARY => -f64::MAX,
            value if value == PARTITION_BOUNDARY || value == PARTITION_BOUNDARY + 1 => f64::MAX,
            _ => 0.0,
        };
        let overflow_float = match id {
            value if value + 1 == PARTITION_BOUNDARY || value == PARTITION_BOUNDARY => f64::MAX,
            value if value == PARTITION_BOUNDARY + 1 => -f64::MAX,
            _ => 0.0,
        };
        write!(
            sql,
            "({id},'bucket_{}','key_{id:05}',{},{}.{:02},{},{safe_sum},{overflow_sum},\
             {safe_float:e},{overflow_float:e})",
            id % 97,
            (id as i64 % 2_003) - 1_001,
            id % 29,
            (id * 25) % 100,
            id % 3 != 0,
        )
        .expect("writing generated SQL");
    }
    sql.push(';');

    let mut database = Database::new();
    database.execute(&sql).expect("generated setup succeeds");
    database
}

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

fn assert_equivalent(database: &mut Database, sql: &str) -> QueryResult {
    let mut expected = None;
    for thread_count in THREAD_COUNTS {
        database
            .set_query_parallelism(thread_count)
            .expect("positive parallelism");
        let actual = query(database, sql);
        if let Some(expected) = &expected {
            assert_eq!(
                &actual, expected,
                "query result changed with {thread_count} workers"
            );
        } else {
            expected = Some(actual);
        }
    }
    expected.expect("at least one thread count")
}

#[test]
fn large_aggregates_are_equivalent_across_thread_counts() {
    let mut database = generated_database();

    assert_equivalent(
        &mut database,
        "SELECT COUNT(*) AS rows, SUM(value) AS total, MIN(value) AS low, \
         MAX(value) AS high, AVG(value) AS mean, SUM(score) AS score_total \
         FROM generated WHERE included = true;",
    );

    let grouped = assert_equivalent(
        &mut database,
        "SELECT bucket, COUNT(*) AS rows, SUM(value) AS total, AVG(score) AS mean \
         FROM generated WHERE included = true GROUP BY bucket;",
    );
    assert_eq!(grouped.rows.len(), 97);

    let high_cardinality = assert_equivalent(
        &mut database,
        "SELECT unique_key, COUNT(*) AS rows, SUM(value) AS total \
         FROM generated GROUP BY unique_key ORDER BY unique_key LIMIT 33000;",
    );
    assert_eq!(high_cardinality.rows.len(), ROW_COUNT);
    assert_eq!(
        high_cardinality.rows.first(),
        Some(&vec![
            Value::String("key_00000".to_owned()),
            Value::Int64(1),
            Value::Int64(-1_001),
        ])
    );
}

#[test]
fn parallel_merge_preserves_overflow_and_empty_input_semantics() {
    let mut database = generated_database();

    let safe = assert_equivalent(
        &mut database,
        "SELECT SUM(safe_sum) AS total FROM generated;",
    );
    assert_eq!(safe.rows, vec![vec![Value::Int64(i64::MAX)]]);

    let safe_float = assert_equivalent(
        &mut database,
        "SELECT SUM(safe_float) AS total, AVG(safe_float) AS mean FROM generated;",
    );
    assert_eq!(
        safe_float.rows,
        vec![vec![
            Value::Float64(f64::MAX),
            Value::Float64(f64::MAX / ROW_COUNT as f64),
        ]]
    );

    let mut expected_error = None;
    for thread_count in THREAD_COUNTS {
        database
            .set_query_parallelism(thread_count)
            .expect("positive parallelism");
        let error = database
            .execute("SELECT SUM(overflow_sum) FROM generated;")
            .expect_err("row-ordered sum overflows");
        assert_eq!(error, Error::NumericOverflow("SUM(Int64)".to_owned()));
        if let Some(expected_error) = &expected_error {
            assert_eq!(&error, expected_error);
        } else {
            expected_error = Some(error);
        }
    }

    for (sql, operation) in [
        ("SELECT SUM(overflow_float) FROM generated;", "SUM(Float64)"),
        (
            "SELECT AVG(overflow_float) FROM generated;",
            "AVG(Float64) sum",
        ),
    ] {
        for thread_count in THREAD_COUNTS {
            database
                .set_query_parallelism(thread_count)
                .expect("positive parallelism");
            let error = database
                .execute(sql)
                .expect_err("row-ordered Float64 accumulation overflows");
            assert_eq!(error, Error::NumericOverflow(operation.to_owned()));
        }
    }

    for (sql, operation) in [
        (
            "SELECT SUM(overflow_sum), SUM(overflow_float) FROM generated;",
            "SUM(Int64)",
        ),
        (
            "SELECT SUM(overflow_float), SUM(overflow_sum) FROM generated;",
            "SUM(Float64)",
        ),
    ] {
        for thread_count in THREAD_COUNTS {
            database
                .set_query_parallelism(thread_count)
                .expect("positive parallelism");
            let error = database
                .execute(sql)
                .expect_err("same-row overflow follows SELECT item order");
            assert_eq!(error, Error::NumericOverflow(operation.to_owned()));
        }
    }

    let global_empty = assert_equivalent(
        &mut database,
        "SELECT COUNT(*) AS rows, SUM(value) AS total FROM generated WHERE id < 0;",
    );
    assert_eq!(
        global_empty.rows,
        vec![vec![Value::Int64(0), Value::Int64(0)]]
    );

    let grouped_empty = assert_equivalent(
        &mut database,
        "SELECT bucket, COUNT(*) AS rows FROM generated WHERE id < 0 GROUP BY bucket;",
    );
    assert!(grouped_empty.rows.is_empty());

    for thread_count in THREAD_COUNTS {
        database
            .set_query_parallelism(thread_count)
            .expect("positive parallelism");
        let error = database
            .execute("SELECT MIN(value) FROM generated WHERE id < 0;")
            .expect_err("MIN remains undefined on empty input");
        assert_eq!(
            error,
            Error::InvalidQuery("MIN is undefined for an empty input".to_owned())
        );
    }
}

#[test]
fn mixed_aggregate_overflow_follows_source_row_order() {
    let mut database = Database::new();
    let setup = format!(
        "CREATE TABLE mixed_overflow (i Int64, f Float64); \
         INSERT INTO mixed_overflow VALUES \
         ({},0.0),(1,0.0),(-1,{:e}),(0,{:e});",
        i64::MAX,
        f64::MAX,
        f64::MAX,
    );
    database.execute(&setup).expect("setup succeeds");

    for sql in [
        "SELECT SUM(i), SUM(f) FROM mixed_overflow;",
        "SELECT SUM(f), SUM(i) FROM mixed_overflow;",
    ] {
        let error = database
            .execute(sql)
            .expect_err("Int64 sum overflows on the earlier source row");
        assert_eq!(error, Error::NumericOverflow("SUM(Int64)".to_owned()));
    }
}

#[test]
fn query_parallelism_configuration_rejects_zero() {
    let mut database = Database::with_query_parallelism(3).expect("positive parallelism");
    assert_eq!(database.query_parallelism(), 3);

    let error = database
        .set_query_parallelism(0)
        .expect_err("zero parallelism");
    assert_eq!(
        error,
        Error::InvalidQuery("query parallelism must be at least 1".to_owned())
    );
    assert_eq!(database.query_parallelism(), 3);
}
