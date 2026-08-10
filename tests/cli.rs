use std::io::Write;
use std::process::{Child, Command, Output, Stdio};

use rusthouse::batch::engine::{Database, StatementResult};
use rusthouse::batch::format::{OutputFormat, render};
use rusthouse::{DEFAULT_MAX_SESSION_BYTES, DEFAULT_MAX_SESSION_STATEMENTS};

fn run(args: &[&str], input: &[u8]) -> Output {
    let mut child = spawn(args);
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

fn run_allowing_stdin_close(args: &[&str], input: &[u8]) -> Output {
    let mut child = spawn(args);
    if let Err(error) = child.stdin.take().unwrap().write_all(input) {
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }
    child.wait_with_output().unwrap()
}

fn spawn(args: &[&str]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

#[test]
fn csv_batch_emits_typed_projection_and_all_scalar_aggregates() {
    let output = run(
        &["--format", "csv"],
        b"CREATE TABLE metrics (
              id Int64,
              score Float64,
              enabled Bool,
              label String
          );
          INSERT INTO metrics VALUES
              (1, 1.5, true, 'semi;colon'),
              (2, 2.5, false, 'comma,value'),
              (3, 4.0, true, 'quote''d');
          SELECT id, score, enabled, label FROM metrics ORDER BY id;
          SELECT COUNT(*) AS row_count,
                 SUM(id) AS id_sum,
                 MIN(score) AS score_min,
                 MAX(score) AS score_max,
                 AVG(score) AS score_avg
          FROM metrics;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        output.stdout,
        b"id,score,enabled,label\n\
          1,1.5,true,semi;colon\n\
          2,2.5,false,\"comma,value\"\n\
          3,4.0,true,quote'd\n\
          row_count,id_sum,score_min,score_max,score_avg\n\
          3,6,1.5,4.0,2.6666666666666665\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn json_cli_executes_conjoined_comparison_delete_silently() {
    let output = run(
        &["--format", "json"],
        b"CREATE TABLE events (id Int64, label String); \
          INSERT INTO events VALUES (1, 'keep'), (2, 'remove'), (3, 'remove'); \
          DELETE FROM events WHERE id >= 2 AND label = 'remove'; \
          SELECT id, label FROM events;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        output.stdout,
        b"{\"columns\":[{\"name\":\"id\",\"type\":\"Int64\"},{\"name\":\"label\",\"type\":\"String\"}],\"rows\":[[1,\"keep\"]]}\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn json_cli_executes_alter_table_delete_silently() {
    let output = run(
        &["--format", "json"],
        b"CREATE TABLE events (id Int64, label String); \
          INSERT INTO events VALUES (1, 'keep'), (2, 'remove'), (3, 'remove'); \
          ALTER TABLE events DELETE WHERE id >= 2 AND label = 'remove'; \
          SELECT id, label FROM events;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        output.stdout,
        b"{\"columns\":[{\"name\":\"id\",\"type\":\"Int64\"},{\"name\":\"label\",\"type\":\"String\"}],\"rows\":[[1,\"keep\"]]}\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn json_cli_outputs_mixed_case_string_to_bool_casts() {
    let output = run(
        &["--format", "json"],
        b"CREATE TABLE flags (id Int64, text String); \
          INSERT INTO flags VALUES (1, 'TRUE'), (2, 'false'), (3, 'FaLsE'); \
          SELECT id, CAST(text AS Bool) AS enabled FROM flags \
          ORDER BY enabled, id;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        output.stdout,
        b"{\"columns\":[{\"name\":\"id\",\"type\":\"Int64\"},{\"name\":\"enabled\",\"type\":\"Bool\"}],\"rows\":[[2,false],[3,false],[1,true]]}\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn json_cli_outputs_to_string_for_every_physical_type() {
    let output = run(
        &["--format", "json"],
        b"CREATE TABLE values_table (i Int64, f Float64, b Bool, s String); \
          INSERT INTO values_table VALUES (-7, -0.0, true, 'Tokyo'); \
          SELECT TOSTRING(i) AS i_text, toString(f) AS f_text, \
                 ToStRiNg(b) AS b_text, tostring(s) AS s_text \
          FROM values_table;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        output.stdout,
        b"{\"columns\":[{\"name\":\"i_text\",\"type\":\"String\"},{\"name\":\"f_text\",\"type\":\"String\"},{\"name\":\"b_text\",\"type\":\"String\"},{\"name\":\"s_text\",\"type\":\"String\"}],\"rows\":[[\"-7\",\"-0\",\"true\",\"Tokyo\"]]}\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn json_cli_output_path_preserves_nullable_int64_to_string_nulls() {
    let mut database = Database::new();
    database
        .create_nullable_int64_table("optional_values", "value", vec![Some(-7), None, Some(0)])
        .expect("setup");
    let results = database
        .execute(
            "SELECT toString(value) AS rendered FROM optional_values \
             ORDER BY rendered",
        )
        .expect("query");
    let [StatementResult::Query(result)] = results.as_slice() else {
        panic!("expected query result")
    };

    assert_eq!(
        render(result, OutputFormat::Json),
        r#"{"columns":[{"name":"rendered","type":"String"}],"rows":[[null],["-7"],["0"]]}"#
    );
}

#[test]
fn json_cli_executes_int64_alter_update_silently() {
    let output = run(
        &["--format", "json"],
        b"CREATE TABLE events (id Int64, value Int64); \
          INSERT INTO events VALUES (1, 10), (2, 20), (2, 30); \
          ALTER TABLE events UPDATE value = -7 WHERE id = 2; \
          SELECT id, value FROM events ORDER BY value;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        output.stdout,
        b"{\"columns\":[{\"name\":\"id\",\"type\":\"Int64\"},{\"name\":\"value\",\"type\":\"Int64\"}],\"rows\":[[2,-7],[2,-7],[1,10]]}\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn json_cli_executes_bool_alter_update_silently() {
    let output = run(
        &["--format", "json"],
        b"CREATE TABLE events (id Int64, active Bool, selected Bool); \
          INSERT INTO events VALUES (1, false, false), (2, false, true), (3, false, true); \
          ALTER TABLE events UPDATE active = true WHERE selected = TRUE; \
          SELECT id, active FROM events ORDER BY id;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        output.stdout,
        b"{\"columns\":[{\"name\":\"id\",\"type\":\"Int64\"},{\"name\":\"active\",\"type\":\"Bool\"}],\"rows\":[[1,false],[2,true],[3,true]]}\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn json_cli_executes_float64_alter_update_silently() {
    let output = run(
        &["--format", "json"],
        b"CREATE TABLE metrics (id Int64, score Float64, selector Float64); \
          INSERT INTO metrics VALUES (1, 1.5, 0.25), (2, 2.5, 0.5); \
          ALTER TABLE metrics UPDATE score = -1.25e2 WHERE selector = 2.5e-1; \
          SELECT id, score FROM metrics ORDER BY id;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        output.stdout,
        b"{\"columns\":[{\"name\":\"id\",\"type\":\"Int64\"},{\"name\":\"score\",\"type\":\"Float64\"}],\"rows\":[[1,-125.0],[2,2.5]]}\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn json_cli_executes_documented_string_alter_update_silently() {
    let output = run(
        &["--format", "json"],
        "CREATE TABLE events (id Int64, label String, category String); \
         INSERT INTO events VALUES (1, 'waiting', 'queued'), (2, 'keep', 'done'); \
         ALTER TABLE events UPDATE label = 'it''s 🚀' WHERE category = 'queued'; \
         ALTER TABLE EVENTS UPDATE CATEGORY = '' WHERE ID = 2; \
         SELECT id, label, category FROM events ORDER BY id;"
            .as_bytes(),
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        output.stdout,
        "{\"columns\":[{\"name\":\"id\",\"type\":\"Int64\"},{\"name\":\"label\",\"type\":\"String\"},{\"name\":\"category\",\"type\":\"String\"}],\"rows\":[[1,\"it's 🚀\",\"queued\"],[2,\"keep\",\"\"]]}\n"
            .as_bytes()
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn json_cli_exposes_typed_defaults_from_reordered_insert_subsets() {
    let output = run(
        &["--format", "json"],
        b"CREATE TABLE metrics (id Int64, score Float64, enabled Bool, label String); \
          INSERT INTO metrics (LABEL, id) VALUES ('first', 1); \
          INSERT INTO metrics (enabled, SCORE) VALUES (true, 2.5); \
          SELECT id, score, enabled, label FROM metrics ORDER BY id;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        output.stdout,
        b"{\"columns\":[{\"name\":\"id\",\"type\":\"Int64\"},{\"name\":\"score\",\"type\":\"Float64\"},{\"name\":\"enabled\",\"type\":\"Bool\"},{\"name\":\"label\",\"type\":\"String\"}],\"rows\":[[0,2.5,true,\"\"],[1,0.0,false,\"first\"]]}\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn csv_batch_pages_filtered_ordered_distinct_tuples() {
    let output = run(
        &["--format", "csv"],
        b"CREATE TABLE events (kind String, rank Int64, score Float64, active Bool); \
          INSERT INTO events VALUES \
              ('alpha', 0, 9.0, false), \
              ('beta', 1, 2.5, true), \
              ('beta', 2, 4.0, true), \
              ('alpha', 7, 0.0, false), \
              ('gamma', 3, 5.0, true); \
          SELECT DISTINCT kind, active FROM events \
          WHERE (active = true AND score >= 2.5) OR rank = 7 \
          ORDER BY active ASC, kind DESC LIMIT 2 OFFSET 1;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"kind,active\ngamma,true\nbeta,true\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn csv_batch_accepts_clickhouse_comma_limit_pagination() {
    let output = run(
        &["--format", "csv"],
        b"CREATE TABLE events (id Int64, active Bool); \
          INSERT INTO events VALUES (4, true), (1, true), (3, false), (2, true), (5, true); \
          SELECT id FROM events WHERE active = true ORDER BY id LIMIT 1, 2;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"id\n2\n4\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn json_cli_executes_contains_like_and_infix_not_like() {
    let output = run(
        &["--format", "json"],
        "CREATE TABLE events (id Int64, label String); \
         INSERT INTO events VALUES (1, '東京'), (2, '東京駅'), (3, 'Alpha'), (4, 'alpha'), (5, '東京'); \
         SELECT id FROM events WHERE label LIKE '%京%' ORDER BY id LIMIT 2; \
         SELECT DISTINCT label FROM events WHERE label NOT LIKE '%lph%' ORDER BY label;"
            .as_bytes(),
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "{\"columns\":[{\"name\":\"id\",\"type\":\"Int64\"}],\"rows\":[[1],[2]]}\n\
         {\"columns\":[{\"name\":\"label\",\"type\":\"String\"}],\"rows\":[[\"東京\"],[\"東京駅\"]]}\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn json_cli_executes_unicode_suffix_like_for_regular_and_distinct_where() {
    let output = run(
        &["--format", "json"],
        "CREATE TABLE events (id Int64, label String); \
         INSERT INTO events VALUES (1, '東京'), (2, '西東京'), (3, '東京駅'), (4, 'Alpha'), (5, 'alpha'), (6, '東京'); \
         SELECT id FROM events WHERE label LIKE '%東京' ORDER BY id LIMIT 2; \
         SELECT DISTINCT label FROM events WHERE NOT label LIKE '%lpha' ORDER BY label;"
            .as_bytes(),
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "{\"columns\":[{\"name\":\"id\",\"type\":\"Int64\"}],\"rows\":[[1],[2]]}\n\
         {\"columns\":[{\"name\":\"label\",\"type\":\"String\"}],\"rows\":[[\"東京\"],[\"東京駅\"],[\"西東京\"]]}\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn json_cli_executes_not_between_for_regular_and_distinct_where() {
    let output = run(
        &["--format", "json"],
        b"CREATE TABLE ranges (id Int64, label String); \
          INSERT INTO ranges VALUES \
          (1, 'outside'), (2, 'inside'), (3, 'inside'), (4, 'edge'), (5, 'outside'); \
          SELECT id FROM ranges WHERE id NOT BETWEEN 2 AND 4 ORDER BY id; \
          SELECT DISTINCT label FROM ranges WHERE id NOT BETWEEN 2 AND 4 ORDER BY label;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        output.stdout,
        b"{\"columns\":[{\"name\":\"id\",\"type\":\"Int64\"}],\"rows\":[[1],[5]]}\n\
          {\"columns\":[{\"name\":\"label\",\"type\":\"String\"}],\"rows\":[[\"outside\"]]}\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn json_cli_executes_typed_in_for_regular_and_distinct_where() {
    let output = run(
        &["--format", "json"],
        b"CREATE TABLE events (id Int64, label String, active Bool); \
          INSERT INTO events VALUES \
          (1, 'one', true), (2, 'two', false), (3, 'three', true), (4, 'two', true); \
          SELECT id FROM events WHERE id IN (1, 3) ORDER BY id; \
          SELECT DISTINCT label FROM events \
          WHERE active NOT IN (false) AND label IN ('two', 'three') ORDER BY label;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        output.stdout,
        b"{\"columns\":[{\"name\":\"id\",\"type\":\"Int64\"}],\"rows\":[[1],[3]]}\n\
          {\"columns\":[{\"name\":\"label\",\"type\":\"String\"}],\"rows\":[[\"three\"],[\"two\"]]}\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn json_cli_projects_ascii_lowercase_strings() {
    let output = run(
        &["--format", "json"],
        "CREATE TABLE samples (label String); \
         INSERT INTO samples VALUES (''), ('MiXeD'), ('ÉCLAIR'), ('東京ABC'); \
         SELECT LOWER(label) AS normalized FROM samples ORDER BY normalized;"
            .as_bytes(),
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "{\"columns\":[{\"name\":\"normalized\",\"type\":\"String\"}],\"rows\":[[\"\"],[\"mixed\"],[\"Éclair\"],[\"東京abc\"]]}\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn tsv_batch_emits_all_types_nulls_empty_and_multiple_escaped_results() {
    let output = run(
        &["--format", "tsv"],
        b"CREATE TABLE metrics (
              id Int64,
              score Float64,
              enabled Bool,
              label String
          );
          SELECT id, score, enabled, label FROM metrics;
          INSERT INTO metrics VALUES
              (1, 1.5, true, 'slash\\tab\tcarriage\rline\nnul\0back\x08form\x0capostrophe''inside'),
              (2, 2.0, false, 'plain');
          SELECT id, score, enabled, label FROM metrics ORDER BY id;
          SELECT MIN(label) AS missing FROM metrics WHERE id < 0;
          SELECT COUNT(*) AS rows FROM metrics;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        output.stdout,
        b"id\tscore\tenabled\tlabel\n\
          id\tscore\tenabled\tlabel\n\
          1\t1.5\ttrue\tslash\\\\tab\\tcarriage\\rline\\nnul\\0back\\bform\\fapostrophe\\'inside\n\
          2\t2.0\tfalse\tplain\n\
          missing\n\
          \\N\n\
          rows\n\
          2\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn table_batch_emits_all_types_empty_and_null_results_in_statement_order() {
    let output = run(
        &["--format", "table"],
        b"SHOW TABLES;
          CREATE TABLE metrics (id Int64, score Float64, enabled Bool, label String);
          INSERT INTO metrics VALUES (7, 1.5, true, 'alpha');
          SELECT id, score, enabled, label FROM metrics;
          SELECT id, score, enabled, label FROM metrics WHERE id < 0;
          SELECT SUM(id) AS i, MIN(score) AS f, MIN(enabled) AS b, MIN(label) AS s
          FROM metrics WHERE id < 0;
          SHOW TABLES;
          DESCRIBE TABLE metrics;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        concat!(
            "+------+\n",
            "| name |\n",
            "+------+\n",
            "+------+\n",
            "\n",
            "+----+-------+---------+-------+\n",
            "| id | score | enabled | label |\n",
            "+----+-------+---------+-------+\n",
            "| 7  | 1.5   | true    | alpha |\n",
            "+----+-------+---------+-------+\n",
            "\n",
            "+----+-------+---------+-------+\n",
            "| id | score | enabled | label |\n",
            "+----+-------+---------+-------+\n",
            "+----+-------+---------+-------+\n",
            "\n",
            "+------+------+------+------+\n",
            "| i    | f    | b    | s    |\n",
            "+------+------+------+------+\n",
            "| NULL | NULL | NULL | NULL |\n",
            "+------+------+------+------+\n",
            "\n",
            "+---------+\n",
            "| name    |\n",
            "+---------+\n",
            "| metrics |\n",
            "+---------+\n",
            "\n",
            "+---------+---------+\n",
            "| name    | type    |\n",
            "+---------+---------+\n",
            "| id      | Int64   |\n",
            "| score   | Float64 |\n",
            "| enabled | Bool    |\n",
            "| label   | String  |\n",
            "+---------+---------+\n",
        )
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn table_batch_rejects_wide_cell_padding_without_partial_output() {
    const ROWS: usize = 10_000;
    let wide_value = "x".repeat(10_000);
    let mut sql =
        format!("CREATE TABLE padded (value String); INSERT INTO padded VALUES ('{wide_value}')");
    for _ in 1..ROWS {
        sql.push_str(",('')");
    }
    sql.push_str("; SELECT value FROM padded;");

    let output = run(&["--format", "table"], sql.as_bytes());

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "error: table output requires at least 100090020 bytes, exceeding the limit of 16777216 bytes\n"
    );
}

#[test]
fn csv_batch_emits_one_left_named_union_all_result() {
    let output = run(
        &["--format", "csv"],
        b"CREATE TABLE first (id Int64, label String);
          CREATE TABLE second (id Int64, label String);
          INSERT INTO first VALUES (1, 'first');
          INSERT INTO second VALUES (2, 'second'), (3, 'third');
          SELECT id AS event_id, label AS description FROM first
          UNION ALL
          SELECT id AS ignored, label AS also_ignored FROM second WHERE id < 3;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"event_id,description\n1,first\n2,second\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn csv_batch_emits_left_named_union_distinct_rows_in_first_seen_order() {
    let output = run(
        &["--format", "csv"],
        b"CREATE TABLE first (id Int64, label String);
          CREATE TABLE second (id Int64, label String);
          INSERT INTO first VALUES (1, 'first'), (1, 'first'), (2, 'second');
          INSERT INTO second VALUES (2, 'second'), (3, 'third'), (3, 'third');
          SELECT id AS event_id, label AS description FROM first
          UNION DISTINCT
          SELECT id AS ignored, label AS also_ignored FROM second;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        output.stdout,
        b"event_id,description\n1,first\n2,second\n3,third\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn csv_batch_emits_typed_cross_join_in_left_major_order() {
    let output = run(
        &["--format", "csv"],
        b"CREATE TABLE left_rows (id Int64, label String);
          CREATE TABLE right_rows (score Float64, active Bool);
          INSERT INTO left_rows VALUES (1, 'first'), (2, 'second');
          INSERT INTO right_rows VALUES (1.5, true), (2.5, false);
          SELECT * FROM left_rows CROSS JOIN right_rows LIMIT 3;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        output.stdout,
        b"id,label,score,active\n\
          1,first,1.5,true\n\
          1,first,2.5,false\n\
          2,second,1.5,true\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn csv_batch_emits_show_tables_metadata_in_stable_display_order() {
    let output = run(
        &["--format", "csv"],
        b"SHOW TABLES IN default;
          CREATE TABLE zebra (id Int64);
          CREATE TABLE Alpha (id Int64);
          CREATE TABLE beta (id Int64);
          sHoW TaBlEs FrOm DeFaUlT;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"name\nname\nAlpha\nbeta\nzebra\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn csv_batch_rejects_non_default_and_trailing_show_tables_syntax() {
    for (sql, expected_error) in [
        (
            "SHOW TABLES FROM analytics;",
            "SHOW TABLES supports only the default database; found 'analytics'",
        ),
        (
            "SHOW TABLES IN default LIMIT 1;",
            "unexpected trailing input after SHOW TABLES",
        ),
    ] {
        let output = run(&["--format", "csv"], sql.as_bytes());

        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains(expected_error)
        );
    }
}

#[test]
fn batch_formats_emit_describe_table_metadata() {
    let sql = b"CREATE TABLE metrics (id Int64, score Float64, active Bool, label String); \
                DESCRIBE TABLE metrics;";

    let csv = run(&["--format", "csv"], sql);
    assert!(csv.status.success(), "{:?}", csv.stderr);
    assert_eq!(
        csv.stdout,
        b"name,type\nid,Int64\nscore,Float64\nactive,Bool\nlabel,String\n"
    );
    assert!(csv.stderr.is_empty());

    let json = run(&["--format", "json"], sql);
    assert!(json.status.success(), "{:?}", json.stderr);
    assert_eq!(
        json.stdout,
        concat!(
            r#"{"columns":[{"name":"name","type":"String"},{"name":"type","type":"String"}],"rows":[["id","Int64"],["score","Float64"],["active","Bool"],["label","String"]]}"#,
            "\n"
        )
        .as_bytes()
    );
    assert!(json.stderr.is_empty());
}

#[test]
fn batch_formats_emit_canonical_show_create_table_ddl() {
    let sql = b"CREATE TABLE Metrics (id int64, score float64, active boolean, label string); \
                SHOW CREATE TABLE metrics;";
    let ddl = "CREATE TABLE Metrics (id Int64, score Float64, active Bool, label String)";

    let csv = run(&["--format", "csv"], sql);
    assert!(csv.status.success(), "{:?}", csv.stderr);
    assert_eq!(
        String::from_utf8(csv.stdout).unwrap(),
        format!("statement\n\"{ddl}\"\n")
    );
    assert!(csv.stderr.is_empty());

    let json = run(&["--format", "json"], sql);
    assert!(json.status.success(), "{:?}", json.stderr);
    assert_eq!(
        String::from_utf8(json.stdout).unwrap(),
        format!(
            "{{\"columns\":[{{\"name\":\"statement\",\"type\":\"String\"}}],\"rows\":[[\"{ddl}\"]]}}\n"
        )
    );
    assert!(json.stderr.is_empty());
}

#[test]
fn json_cli_round_trips_sql_created_nullable_int64_values_and_ddl() {
    let output = run(
        &["--format", "json"],
        b"CREATE TABLE Readings (measurement Nullable(Int64)); \
          INSERT INTO readings VALUES (7), (NULL), (-2); \
          SHOW CREATE TABLE READINGS; \
          SELECT measurement FROM readings ORDER BY measurement ASC;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        output.stdout,
        concat!(
            r#"{"columns":[{"name":"statement","type":"String"}],"rows":[["CREATE TABLE Readings (measurement Nullable(Int64))"]]}"#,
            "\n",
            r#"{"columns":[{"name":"measurement","type":"Int64"}],"rows":[[null],[-2],[7]]}"#,
            "\n",
        )
        .as_bytes()
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn batch_cli_keeps_drop_table_command_output_silent() {
    for format in [
        "table",
        "csv",
        "tsv",
        "json",
        "JSONEachRow",
        "JSONCompactEachRow",
    ] {
        let output = run(
            &["--format", format],
            b"CREATE TABLE temporary (id Int64); DROP TABLE TEMPORARY;",
        );

        assert!(output.status.success(), "{format}: {:?}", output.stderr);
        assert!(output.stdout.is_empty(), "{format}");
        assert!(output.stderr.is_empty(), "{format}");
    }
}

#[test]
fn batch_cli_supports_an_idempotent_conditional_drop_lifecycle() {
    let output = run(
        &["--format", "tsv"],
        b"DROP TABLE IF EXISTS temporary; \
          CREATE TABLE temporary (id Int64); \
          INSERT INTO temporary VALUES (1); \
          DROP TABLE IF EXISTS TEMPORARY; \
          drop table if exists temporary; \
          EXISTS TABLE temporary;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"result\nfalse\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn batch_cli_keeps_truncate_table_command_output_silent() {
    for format in [
        "table",
        "csv",
        "tsv",
        "json",
        "JSONEachRow",
        "JSONCompactEachRow",
    ] {
        let output = run(
            &["--format", format],
            b"CREATE TABLE temporary (id Int64); \
              INSERT INTO temporary VALUES (1), (2); \
              TRUNCATE TABLE TEMPORARY;",
        );

        assert!(output.status.success(), "{format}: {:?}", output.stderr);
        assert!(output.stdout.is_empty(), "{format}");
        assert!(output.stderr.is_empty(), "{format}");
    }
}

#[test]
fn csv_batch_observes_the_complete_rename_table_lifecycle() {
    let output = run(
        &["--format", "csv"],
        b"CREATE TABLE OldName (id Int64, label String); \
          INSERT INTO OldName VALUES (7, 'kept'); \
          RENAME TABLE oldname TO NewName; \
          SELECT id, label FROM newname; \
          SHOW TABLES; \
          SHOW CREATE TABLE NEWNAME;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        output.stdout,
        b"id,label\n\
          7,kept\n\
          name\n\
          NewName\n\
          statement\n\
          \"CREATE TABLE NewName (id Int64, label String)\"\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn csv_batch_observes_a_renamed_column_in_data_and_schema_queries() {
    let output = run(
        &["--format", "csv"],
        b"CREATE TABLE Metrics (id Int64, score Float64); \
          INSERT INTO metrics VALUES (7, 2.5); \
          ALTER TABLE METRICS RENAME COLUMN SCORE TO Rating; \
          SELECT id, rating FROM metrics; \
          SHOW CREATE TABLE metrics; \
          DESCRIBE TABLE metrics;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        output.stdout,
        b"id,Rating\n\
          7,2.5\n\
          statement\n\
          \"CREATE TABLE Metrics (id Int64, Rating Float64)\"\n\
          name,type\n\
          id,Int64\n\
          Rating,Float64\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn csv_batch_observes_added_columns_defaults_and_metadata() {
    let output = run(
        &["--format", "csv"],
        b"CREATE TABLE Metrics (id Int64); \
          INSERT INTO metrics VALUES (7); \
          ALTER TABLE METRICS ADD COLUMN score Float64; \
          ALTER TABLE metrics ADD COLUMN active Bool; \
          ALTER TABLE Metrics ADD COLUMN label String; \
          SELECT id, score, active, label FROM metrics; \
          SHOW CREATE TABLE metrics; \
          DESCRIBE TABLE metrics;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        output.stdout,
        b"id,score,active,label\n\
          7,0.0,false,\n\
          statement\n\
          \"CREATE TABLE Metrics (id Int64, score Float64, active Bool, label String)\"\n\
          name,type\n\
          id,Int64\n\
          score,Float64\n\
          active,Bool\n\
          label,String\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn csv_batch_observes_a_dropped_column_in_data_and_schema_queries() {
    let output = run(
        &["--format", "csv"],
        b"CREATE TABLE Metrics (id Int64, score Float64, active Bool); \
          INSERT INTO metrics VALUES (7, 2.5, true); \
          ALTER TABLE METRICS DROP COLUMN SCORE; \
          SELECT id, active FROM metrics; \
          SHOW CREATE TABLE metrics; \
          DESCRIBE TABLE metrics;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        output.stdout,
        b"id,active\n\
          7,true\n\
          statement\n\
          \"CREATE TABLE Metrics (id Int64, active Bool)\"\n\
          name,type\n\
          id,Int64\n\
          active,Bool\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn table_batch_observes_truncate_between_query_results() {
    let output = run(
        &["--format", "table"],
        b"CREATE TABLE events (id Int64); \
          INSERT INTO events VALUES (1), (2); \
          SELECT COUNT(*) AS rows FROM events; \
          TRUNCATE TABLE events; \
          SELECT COUNT(*) AS rows FROM events;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        concat!(
            "+------+\n",
            "| rows |\n",
            "+------+\n",
            "| 2    |\n",
            "+------+\n",
            "\n",
            "+------+\n",
            "| rows |\n",
            "+------+\n",
            "| 0    |\n",
            "+------+\n",
        )
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn csv_batch_filters_grouped_rows_with_a_count_alias() {
    let output = run(
        &["--format", "csv"],
        b"CREATE TABLE events (kind String, active Bool);
          INSERT INTO events VALUES
              ('a', true), ('a', true), ('a', false),
              ('b', true), ('b', false), ('c', true), ('c', true);
          SELECT kind, COUNT(*) AS Occurrences FROM events
          WHERE active = true
          GROUP BY kind
          HAVING occurrences >= 2
          ORDER BY occurrences DESC, kind DESC
          LIMIT 1;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"kind,Occurrences\nc,2\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn csv_batch_accepts_empty_count_with_grouping_having_ordering_and_pagination() {
    let output = run(
        &["--format", "csv"],
        b"CREATE TABLE events (kind String, included Bool);
          INSERT INTO events VALUES
              ('a', true), ('a', true), ('a', false),
              ('b', true), ('b', true), ('c', true);
          SELECT kind, count() AS occurrences FROM events
          WHERE included = true
          GROUP BY kind
          HAVING occurrences >= 2
          ORDER BY occurrences DESC, kind DESC
          LIMIT 1 OFFSET 1;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"kind,occurrences\na,2\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn csv_batch_executes_grouped_count_if_with_alias_having_and_pagination() {
    let output = run(
        &["--format", "csv"],
        b"CREATE TABLE events (kind String, active Bool, included Bool);
          INSERT INTO events VALUES
              ('a', true, true), ('a', false, true),
              ('b', true, true), ('b', true, true),
              ('c', false, true), ('ignored', true, false);
          SELECT kind, countIf(active) AS true_count FROM events
          WHERE included = true GROUP BY kind HAVING true_count >= 1
          ORDER BY true_count DESC, kind ASC LIMIT 1 OFFSET 1;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"kind,true_count\na,1\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn csv_batch_filters_grouped_rows_with_a_float64_aggregate_alias() {
    let output = run(
        &["--format", "csv"],
        b"CREATE TABLE events (kind String, score Float64);
          INSERT INTO events VALUES
              ('a', 1.0), ('a', 2.0),
              ('b', 4.0), ('b', 6.0),
              ('c', 9.0);
          SELECT kind, AVG(score) AS mean FROM events
          GROUP BY kind
          HAVING mean >= 1.5
          ORDER BY mean DESC
          LIMIT 2;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"kind,mean\nc,9.0\nb,5.0\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn csv_batch_emits_typed_nulls_for_empty_aggregate_inputs() {
    let output = run(
        &["--format", "csv"],
        b"CREATE TABLE samples (id Int64, score Float64, label String);
          SELECT COUNT(*) AS rows, SUM(id) AS total, MIN(label) AS first,
                 MAX(score) AS high, AVG(score) AS mean FROM samples;
          INSERT INTO samples VALUES (1, 2.5, 'present');
          SELECT COUNT(*) AS rows, SUM(id) AS total, MIN(label) AS first,
                 MAX(score) AS high, AVG(score) AS mean
          FROM samples WHERE id < 0;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        output.stdout,
        b"rows,total,first,high,mean\n0,NULL,NULL,NULL,NULL\n\
          rows,total,first,high,mean\n0,NULL,NULL,NULL,NULL\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn batch_formats_apply_having_nullness_to_empty_and_populated_aggregates() {
    let sql = b"CREATE TABLE samples (value Int64);
        SELECT SUM(value) AS total FROM samples
        HAVING total IS NULL ORDER BY total LIMIT 1;
        INSERT INTO samples VALUES (7);
        SELECT SUM(value) AS total FROM samples
        HAVING total IS NOT NULL ORDER BY total DESC LIMIT 1;";
    let cases = [
        ("csv", "total\nNULL\ntotal\n7\n"),
        ("tsv", "total\n\\N\ntotal\n7\n"),
        (
            "json",
            concat!(
                "{\"columns\":[{\"name\":\"total\",\"type\":\"Int64\"}],\"rows\":[[null]]}\n",
                "{\"columns\":[{\"name\":\"total\",\"type\":\"Int64\"}],\"rows\":[[7]]}\n",
            ),
        ),
        (
            "table",
            concat!(
                "+-------+\n",
                "| total |\n",
                "+-------+\n",
                "| NULL  |\n",
                "+-------+\n",
                "\n",
                "+-------+\n",
                "| total |\n",
                "+-------+\n",
                "| 7     |\n",
                "+-------+\n",
            ),
        ),
    ];

    for (format, expected) in cases {
        let output = run(&["--format", format], sql);
        assert!(output.status.success(), "{format}: {:?}", output.stderr);
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            expected,
            "{format}"
        );
        assert!(output.stderr.is_empty(), "{format}");
    }
}

#[test]
fn json_batch_emits_escaped_typed_results_null_aggregates_and_show_tables() {
    let output = run(
        &["--format", "json"],
        br#"CREATE TABLE metrics (
              id Int64,
              score Float64,
              enabled Bool,
              label String
          );
          INSERT INTO metrics VALUES
              (1, 1.5, true, 'quote" and slash\
line	tab'),
              (2, 2.5, false, 'plain');
          SELECT id, score, enabled, label FROM metrics ORDER BY id;
          SELECT COUNT(*) AS row_count,
                 SUM(id) AS id_sum,
                 MIN(label) AS label_min,
                 MAX(score) AS score_max,
                 AVG(score) AS score_avg
          FROM metrics WHERE id < 0;
          SHOW TABLES;"#,
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        output.stdout,
        concat!(
            r#"{"columns":[{"name":"id","type":"Int64"},{"name":"score","type":"Float64"},{"name":"enabled","type":"Bool"},{"name":"label","type":"String"}],"rows":[[1,1.5,true,"quote\" and slash\\\nline\ttab"],[2,2.5,false,"plain"]]}"#,
            "\n",
            r#"{"columns":[{"name":"row_count","type":"Int64"},{"name":"id_sum","type":"Int64"},{"name":"label_min","type":"String"},{"name":"score_max","type":"Float64"},{"name":"score_avg","type":"Float64"}],"rows":[[0,null,null,null,null]]}"#,
            "\n",
            r#"{"columns":[{"name":"name","type":"String"}],"rows":[["metrics"]]}"#,
            "\n"
        )
        .as_bytes()
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn json_compact_each_row_emits_all_types_empty_and_multiple_results() {
    let output = run(
        &["--format", "JSONCompactEachRow"],
        br#"CREATE TABLE metrics (
              id Int64,
              score Float64,
              enabled Bool,
              label String
          );
          SELECT id, score, enabled, label FROM metrics;
          INSERT INTO metrics VALUES
              (1, 1.5, true, 'quote" and slash\
line	tab'),
              (2, 2.0, false, 'plain');
          SELECT id, score, enabled, label FROM metrics ORDER BY id;
          SELECT MIN(id), MIN(score), MIN(enabled), MIN(label)
          FROM metrics WHERE id < 0;"#,
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        output.stdout,
        concat!(
            r#"[1,1.5,true,"quote\" and slash\\\nline\ttab"]"#,
            "\n",
            r#"[2,2.0,false,"plain"]"#,
            "\n",
            "[null,null,null,null]\n",
        )
        .as_bytes()
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn json_each_row_emits_all_types_empty_and_multiple_results() {
    let output = run(
        &["--format", "JSONEachRow"],
        br#"CREATE TABLE metrics (
              id Int64,
              score Float64,
              enabled Bool,
              label String
          );
          SELECT id, score, enabled, label FROM metrics;
          INSERT INTO metrics VALUES
              (1, 1.5, true, 'quote" and slash\
line	tab'),
              (2, 2.0, false, 'plain');
          SELECT id AS identifier, score, enabled, label AS text_name
          FROM metrics ORDER BY identifier;
          SELECT MIN(id) AS missing FROM metrics WHERE id < 0;"#,
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        output.stdout,
        concat!(
            r#"{"identifier":1,"score":1.5,"enabled":true,"text_name":"quote\" and slash\\\nline\ttab"}"#,
            "\n",
            r#"{"identifier":2,"score":2.0,"enabled":false,"text_name":"plain"}"#,
            "\n",
            "{\"missing\":null}\n",
        )
        .as_bytes()
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn fixed_harness_style_write_completes_without_early_exit_or_broken_pipe() {
    const ROWS: usize = 4_096;
    let mut sql = String::from(
        "CREATE TABLE parity_data (id Int64, score Float64, flag Bool, label String);\n\
         INSERT INTO parity_data VALUES ",
    );
    for row in 0..ROWS {
        if row != 0 {
            sql.push(',');
        }
        let flag = row % 2 == 0;
        sql.push_str(&format!("({row},{}.5,{flag},'row_{row:05}')", row % 100));
    }
    sql.push_str(
        ";\nSELECT COUNT(*) AS row_count, SUM(id) AS total, MIN(score) AS low, \
         MAX(score) AS high, AVG(score) AS mean FROM parity_data;\n\
         SELECT COUNT(*) AS row_count FROM parity_data;\n",
    );
    assert!(
        sql.len() > 64 * 1024,
        "input must exceed a typical pipe buffer"
    );

    let mut child = spawn(&["--format", "csv"]);
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(sql.as_bytes())
        .expect("the process must keep stdin open for the complete batch");
    drop(stdin);
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success(), "{:?}", output.stderr);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("row_count,total,low,high,mean\n4096,8386560,"));
    assert!(stdout.ends_with("row_count\n4096\n"));
    assert!(output.stderr.is_empty());
}

#[test]
fn csv_batch_rejects_repeated_oversized_projections_before_materialization() {
    let mut sql = String::from("CREATE TABLE many_rows (flag Bool); INSERT INTO many_rows VALUES ");
    for row in 0..20_000 {
        if row != 0 {
            sql.push(',');
        }
        sql.push_str("(true)");
    }
    sql.push(';');
    for _ in 0..200 {
        sql.push_str("SELECT flag FROM many_rows;");
    }

    let output = run(&["--format", "csv"], sql.as_bytes());

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("SELECT result rows requires at least 20000"));
    assert!(stderr.contains("exceeding the limit of 10000"));
}

#[test]
fn only_exact_supported_format_arguments_are_accepted() {
    for args in [
        &["--format", "TABLE"][..],
        &["--format", "CSV"][..],
        &["--format", "JSON"][..],
        &["--format", "TSV"][..],
        &["--format", "jsoneachrow"][..],
        &["--format", "JSONEACHROW"][..],
        &["--format", "JSONCOMPACTEACHROW"][..],
        &["--format", "table", "extra"][..],
        &["--format", "csv", "extra"][..],
        &["--format", "json", "extra"][..],
        &["--format", "tsv", "extra"][..],
        &["--format", "JSONEachRow", "extra"][..],
        &["--format", "JSONCompactEachRow", "extra"][..],
    ] {
        let output = run(args, b"");
        assert!(!output.status.success(), "{args:?}");
    }
}

#[test]
fn executes_a_catalog_lifecycle_and_formats_nullable_rows() {
    let output = run(
        &[],
        b"\nCREATE TABLE readings (value Int64)\r\n\
          INSERT INTO readings VALUES (7)\n\
          INSERT INTO readings VALUES (NULL)\n\
          INSERT INTO readings VALUES (-2)\n\
          SELECT value FROM readings\n",
    );

    assert!(output.status.success());
    assert_eq!(output.stdout, b"[7, NULL, -2]\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn prints_each_select_result_in_statement_order() {
    let output = run(
        &[],
        b"CREATE TABLE readings (value Int64 NOT NULL)\n\
          INSERT INTO readings VALUES (3)\n\
          SELECT value FROM readings\n\
          INSERT INTO readings VALUES (5)\n\
          SELECT value FROM readings WHERE value >= 5\n\
          SELECT value FROM readings LIMIT 0\n",
    );

    assert!(output.status.success());
    assert_eq!(output.stdout, b"[3]\n[5]\n[]\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn legacy_cli_executes_nullable_inner_join_in_deterministic_cross_product_order() {
    let output = run(
        &[],
        b"CREATE TABLE left_rows (left_key Int64)\n\
          CREATE TABLE right_rows (right_key Int64)\n\
          INSERT INTO left_rows VALUES (7)\n\
          INSERT INTO left_rows VALUES (NULL)\n\
          INSERT INTO left_rows VALUES (7)\n\
          INSERT INTO left_rows VALUES (8)\n\
          INSERT INTO right_rows VALUES (7)\n\
          INSERT INTO right_rows VALUES (7)\n\
          INSERT INTO right_rows VALUES (NULL)\n\
          INSERT INTO right_rows VALUES (8)\n\
          SELECT left_key FROM left_rows INNER JOIN right_rows ON left_key = right_key;\n",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"[7, 7, 7, 7, 8]\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn legacy_cli_executes_left_join_with_duplicates_nulls_and_empty_inputs() {
    let output = run(
        &[],
        b"CREATE TABLE left_rows (left_key Int64)\n\
          CREATE TABLE right_rows (right_key Int64)\n\
          SELECT right_key FROM left_rows LEFT JOIN right_rows ON left_key = right_key;\n\
          INSERT INTO left_rows VALUES (7)\n\
          INSERT INTO left_rows VALUES (NULL)\n\
          INSERT INTO left_rows VALUES (8)\n\
          INSERT INTO right_rows VALUES (7)\n\
          INSERT INTO right_rows VALUES (7)\n\
          INSERT INTO right_rows VALUES (NULL)\n\
          SELECT right_key FROM left_rows LEFT JOIN right_rows ON left_key = right_key;\n",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"[]\n[7, 7, NULL, NULL]\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn legacy_cli_reports_unknown_left_join_identifiers() {
    let output = run(
        &[],
        b"CREATE TABLE left_rows (left_key Int64)\n\
          CREATE TABLE right_rows (right_key Int64)\n\
          SELECT missing FROM left_rows LEFT JOIN right_rows ON left_key = right_key;\n",
    );

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("could not execute LEFT JOIN: unknown column 'missing'")
    );
}

#[test]
fn help_prints_usage_without_reading_a_session() {
    for argument in ["--help", "-h"] {
        let output = run_allowing_stdin_close(&[argument], b"not SQL\n");

        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("Usage: rusthouse [OPTIONS]"));
        assert!(stdout.contains("--format table"));
        assert!(stdout.contains("--format tsv"));
        assert!(stdout.contains("--format json"));
        assert!(stdout.contains("--format JSONEachRow"));
        assert!(stdout.contains("--format JSONCompactEachRow"));
        assert!(stdout.contains("65536 input bytes, 1024 statements, 64 tables"));
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn malformed_statement_is_reported_on_stderr_with_failure_status() {
    let output = run(
        &[],
        b"CREATE TABLE readings (value Int64)\nSELECT FROM readings\n",
    );

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.starts_with("error: line 2: could not parse SQL:"));
    assert!(stderr.contains("expected identifier"));
}

#[test]
fn cli_conditional_create_is_silent_and_preserves_the_existing_table_lifecycle() {
    let output = run(
        &["--format", "csv"],
        b"CREATE TABLE IF NOT EXISTS Events (id Int64, label String); \
          INSERT INTO Events VALUES (1, 'original'); \
          CREATE TABLE IF NOT EXISTS eVeNtS (replacement Bool); \
          INSERT INTO events VALUES (2, 'retained'); \
          SELECT id, label FROM EVENTS ORDER BY id;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"id,label\n1,original\n2,retained\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn cli_accepts_memory_engine_for_plain_and_conditional_create() {
    let output = run(
        &["--format", "csv"],
        b"CREATE TABLE Events (id Int64) ENGINE = Memory; \
          CREATE TABLE IF NOT EXISTS events (ignored String) engine=memory; \
          INSERT INTO events VALUES (7); \
          SELECT database, name, engine, total_rows FROM system.tables;",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        output.stdout,
        b"database,name,engine,total_rows\ndefault,Events,Memory,1\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn cli_rejects_invalid_engine_suffix_before_executing_the_batch() {
    for suffix in [
        "ENGINE Memory",
        "ENGINE = MergeTree",
        "ENGINE = Memory ENGINE = Memory",
        "ENGINE = Memory trailing",
    ] {
        let input = format!(
            "CREATE TABLE first (id Int64); \
             SELECT database, name, engine, total_rows FROM system.tables; \
             CREATE TABLE rejected (id Int64) {suffix};"
        );
        let output = run(&["--format", "csv"], input.as_bytes());

        assert!(
            !output.status.success(),
            "invalid suffix succeeded: {suffix:?}"
        );
        assert!(
            output.stdout.is_empty(),
            "batch executed before failure: {suffix:?}"
        );
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .starts_with("error: SQL error at byte "),
            "{suffix:?}"
        );
    }
}

#[test]
fn legacy_cli_conditional_create_preserves_rows_across_case_variants() {
    let output = run(
        &[],
        b"CREATE TABLE IF NOT EXISTS Events (value Int64) ENGINE = Memory\n\
          INSERT INTO Events VALUES (7)\n\
          CREATE TABLE IF NOT EXISTS events (replacement Int64 NOT NULL) engine=memory\n\
          SELECT value FROM Events\n",
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"[7]\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn accepts_exact_session_byte_bound() {
    let input = vec![b' '; DEFAULT_MAX_SESSION_BYTES];
    let output = run(&[], &input);

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn rejects_exceeded_session_byte_bound() {
    let input = vec![b' '; DEFAULT_MAX_SESSION_BYTES + 1];
    let output = run(&[], &input);

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!(
            "error: session input has at least {} bytes, exceeding the limit of {} bytes\n",
            DEFAULT_MAX_SESSION_BYTES + 1,
            DEFAULT_MAX_SESSION_BYTES
        )
    );
}

#[test]
fn accepts_exact_session_statement_bound() {
    let mut input = String::from("CREATE TABLE readings (value Int64 NOT NULL)\n");
    for _ in 1..DEFAULT_MAX_SESSION_STATEMENTS {
        input.push_str("INSERT INTO readings VALUES (1)\n");
    }
    let output = run(&[], input.as_bytes());

    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn rejects_exceeded_session_statement_bound() {
    let mut input = String::from("CREATE TABLE readings (value Int64 NOT NULL)\n");
    for _ in 1..DEFAULT_MAX_SESSION_STATEMENTS {
        input.push_str("INSERT INTO readings VALUES (1)\n");
    }
    input.push_str("SELECT value FROM readings LIMIT 0\n");
    let output = run(&[], input.as_bytes());

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!(
            "error: line {} raises the session to {} statements, exceeding the limit of {}\n",
            DEFAULT_MAX_SESSION_STATEMENTS + 1,
            DEFAULT_MAX_SESSION_STATEMENTS + 1,
            DEFAULT_MAX_SESSION_STATEMENTS
        )
    );
}
