use rusthouse::{Database, QueryResult, StatementResult, Value};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    match database
        .execute(sql)
        .expect("query succeeds")
        .into_iter()
        .last()
        .expect("one result")
    {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query"),
    }
}

#[test]
fn equality_filters_skip_fixed_and_partial_granules_with_result_parity() {
    let rows = "(1, 1, 'one'), (2, 2, 'two'), (3, 3, 'three'),
                (4, 4, 'four'), (5, 18446744073709551615, 'five')";
    let mut indexed = Database::new();
    indexed
        .execute(&format!(
            "CREATE TABLE events (
                id Int64,
                sequence UInt64,
                label String,
                INDEX id_bloom id TYPE bloom_filter(0.000001) GRANULARITY 2,
                INDEX sequence_bloom sequence TYPE bloom_filter(0.000001) GRANULARITY 2,
                INDEX label_bloom label TYPE bloom_filter(0.000001) GRANULARITY 2
             ); INSERT INTO events VALUES {rows};"
        ))
        .expect("indexed setup");
    let mut plain = Database::new();
    plain
        .execute(&format!(
            "CREATE TABLE events (id Int64, sequence UInt64, label String);
             INSERT INTO events VALUES {rows};"
        ))
        .expect("plain setup");

    for sql in [
        "SELECT id, label FROM events WHERE id = 5",
        "SELECT id, label FROM events WHERE sequence = 18446744073709551615",
        "SELECT id, label FROM events WHERE label = 'three'",
        "SELECT id, label FROM events WHERE id = 99 OR label = 'four'",
        "SELECT id, label FROM events WHERE id = 3 AND label = 'missing'",
    ] {
        assert_eq!(query(&mut indexed, sql), query(&mut plain, sql), "{sql}");
    }

    let result = query(&mut indexed, "SELECT id FROM events WHERE id = 999999");
    assert!(result.rows.is_empty());
    let stats = indexed.last_scan_stats().expect("scan statistics");
    assert_eq!(stats.total_rows, 5);
    assert_eq!(stats.total_granules, 3);
    assert!(stats.scanned_rows < stats.total_rows);
    assert_eq!(stats.scanned_rows + stats.skipped_rows, stats.total_rows);
    assert_eq!(
        indexed.catalog().table("events").expect("table").indexes()[0].granule_count(),
        3
    );
}

#[test]
fn forced_false_positives_scan_rows_but_do_not_change_results() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE valueset (
                value String,
                INDEX value_bloom value TYPE bloom_filter(0.999) GRANULARITY 1
             );
             INSERT INTO valueset VALUES ('alpha'), ('beta'), ('gamma');",
        )
        .expect("setup");

    let result = query(
        &mut database,
        "SELECT value FROM valueset WHERE value = 'not-present'",
    );
    assert!(result.rows.is_empty());
    let stats = database.last_scan_stats().expect("scan statistics");
    assert_eq!(stats.scanned_rows, 3);
    assert_eq!(stats.skipped_rows, 0);
}

#[test]
fn indexes_build_over_existing_rows_rebuild_and_track_later_inserts() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (id Int64, label String);
             INSERT INTO events VALUES (1, 'one'), (2, 'two'), (3, 'three');
             CREATE INDEX label_bloom ON events (label)
                TYPE bloom_filter(0.000001) GRANULARITY 2;
             ALTER TABLE events MATERIALIZE INDEX label_bloom;
             INSERT INTO events VALUES (4, 'four'), (5, 'five');",
        )
        .expect("build and rebuild index");

    assert_eq!(
        query(
            &mut database,
            "SELECT id FROM events WHERE label = 'five' ORDER BY id",
        )
        .rows,
        vec![vec![Value::Int64(5)]]
    );
    assert_eq!(
        database.catalog().table("events").expect("table").indexes()[0].granule_count(),
        3
    );

    let absent = query(
        &mut database,
        "SELECT id FROM events WHERE label = 'absent'",
    );
    assert!(absent.rows.is_empty());
    assert!(
        database
            .last_scan_stats()
            .expect("scan statistics")
            .skipped_rows
            > 0
    );
}

#[test]
fn failed_sql_insert_leaves_bloom_indexes_unchanged() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (
                id Int64,
                label String,
                INDEX id_bloom id TYPE bloom_filter(0.01) GRANULARITY 2
             );
             INSERT INTO events VALUES (1, 'one'), (2, 'two');",
        )
        .expect("setup");

    database
        .execute("INSERT INTO events VALUES (3, 'three'), (4, false)")
        .expect_err("invalid second row");

    let table = database.catalog().table("events").expect("table");
    assert_eq!(table.row_count(), 2);
    assert_eq!(table.indexes()[0].granule_count(), 1);
    assert!(
        query(&mut database, "SELECT id FROM events WHERE id = 3")
            .rows
            .is_empty()
    );
}
