use std::fmt::Write as _;

use rusthouse::{Database, Error, QueryResult, StatementResult, Value};

const ROW_COUNT: usize = 33_000;
const PARTITION_BOUNDARY: usize = 16 * 1_024;
const THREAD_COUNTS: [usize; 4] = [1, 2, 4, 8];

fn generated_database() -> Database {
    let mut sql = String::with_capacity(ROW_COUNT * 80);
    sql.push_str(
        "CREATE TABLE generated (id Int64, bucket String, unique_key String, value Int64, \
         score Float64, included Bool, safe_sum Int64, overflow_sum Int64); \
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
        write!(
            sql,
            "({id},'bucket_{}','key_{id:05}',{},{}.{:02},{},{safe_sum},{overflow_sum})",
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
