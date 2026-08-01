use std::io::Write;
use std::process::{Command, Output, Stdio};

fn run_cli(sql: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args(["--format", "csv"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rusthouse");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(sql.as_bytes())
        .expect("write SQL");
    child.wait_with_output().expect("wait for rusthouse")
}

fn successful_stdout(sql: &str) -> String {
    let output = run_cli(sql);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("UTF-8 stdout")
}

#[test]
fn numeric_schema_filters_projects_and_aggregates() {
    let stdout = successful_stdout(
        "CREATE TABLE metrics (id Int64, value Float64) ENGINE = Memory;
         INSERT INTO metrics VALUES (1, 1.5), (2, 4.0), (3, 2.5), (4, 8.0);
         SELECT id AS key, value * 2 AS doubled
           FROM metrics
          WHERE (id >= 2 AND value < 8.0) OR id = 1
          ORDER BY doubled DESC, key ASC LIMIT 3;
         SELECT count() AS rows, sum(value) AS total, min(value) AS low,
                max(value) AS high, avg(value) AS mean FROM metrics;",
    );
    assert_eq!(
        stdout,
        "\"key\",\"doubled\"\n2,8\n3,5\n1,3\n\
         \"rows\",\"total\",\"low\",\"high\",\"mean\"\n4,16,1.5,8,4\n"
    );
}

#[test]
fn categorical_schema_groups_and_uses_boolean_predicates() {
    let stdout = successful_stdout(
        "CREATE TABLE facts (
             event_id Int64, category String, amount Int64, enabled Bool
         );
         INSERT INTO facts VALUES
             (1, 'red', 10, 1), (2, 'blue', 5, 1),
             (3, 'red', 7, 0), (4, 'blue', 12, 1),
             (5, 'green', 20, 1);
         SELECT category AS bucket, count(*) AS n, sum(amount) AS total
           FROM facts WHERE enabled = 1
          GROUP BY category HAVING total >= 10
          ORDER BY total DESC, bucket ASC;",
    );
    assert_eq!(
        stdout,
        "\"bucket\",\"n\",\"total\"\n\"green\",1,20\n\"blue\",2,17\n\"red\",1,10\n"
    );
}

#[test]
fn nullable_mixed_schema_and_escaped_strings_round_trip() {
    let stdout = successful_stdout(
        r#"CREATE TABLE wide (
               id Int64, ratio Float64, healthy Bool, label String,
               optional_score Nullable(Int64)
           );
           INSERT INTO wide VALUES
               (1, 1.25, true, 'O''Brien', NULL),
               (2, 2.5, false, 'comma,value', 9),
               (3, 3.75, true, 'quote"value', NULL),
               (4, 4.0, true, 'back\slash', 2);
           SELECT id, label AS text, optional_score
             FROM wide
            WHERE optional_score IS NULL AND (healthy = true OR ratio > 10)
            ORDER BY id ASC;
           SELECT count(optional_score) AS present FROM wide;"#,
    );
    assert_eq!(
        stdout,
        "\"id\",\"text\",\"optional_score\"\n1,\"O'Brien\",\\N\n3,\"quote\"\"value\",\\N\n\
         \"present\"\n2\n"
    );
}

#[test]
fn repeated_queries_keep_the_session_and_each_emit_names() {
    let stdout = successful_stdout(
        "CREATE TABLE t (id Int64, name String);
         INSERT INTO t VALUES (2, 'two'), (1, 'one'), (3, 'three');
         SELECT name FROM t ORDER BY id LIMIT 1;
         SELECT name FROM t ORDER BY id DESC LIMIT 1;
         SELECT DISTINCT name FROM t ORDER BY name LIMIT 2 OFFSET 1;",
    );
    assert_eq!(
        stdout,
        "\"name\"\n\"one\"\n\"name\"\n\"three\"\n\"name\"\n\"three\"\n\"two\"\n"
    );
}

#[test]
fn accepts_a_large_multi_row_values_batch() {
    let mut sql =
        String::from("CREATE TABLE bulk (id Int64, value Int64); INSERT INTO bulk VALUES ");
    for id in 0..20_000 {
        if id > 0 {
            sql.push(',');
        }
        sql.push_str(&format!("({id},{})", id % 17));
    }
    sql.push_str("; SELECT count(*) AS n, sum(value) AS total FROM bulk;");
    assert_eq!(successful_stdout(&sql), "\"n\",\"total\"\n20000,159964\n");
}

#[test]
fn malformed_input_returns_a_typed_error_without_stdout() {
    let output =
        run_cli("CREATE TABLE bad (id Int64); INSERT INTO bad VALUES (1; SELECT * FROM bad;");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("parse error at byte"), "{stderr}");
    assert!(stderr.contains("expected ')'"), "{stderr}");
}
