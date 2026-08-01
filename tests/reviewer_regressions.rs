use rusthouse::{DataType, Database, Error, Value};

#[test]
fn flat_expression_chains_hit_a_typed_node_limit() {
    let sql = format!("SELECT {};", vec!["1"; 20_000].join("+"));
    let error = Database::new().execute(&sql).unwrap_err();
    assert!(matches!(
        error,
        Error::Limit {
            resource: "SQL expression nodes",
            ..
        }
    ));
}

#[test]
fn unicode_after_a_backslash_consumes_a_complete_scalar() {
    let result = Database::new()
        .execute(r"SELECT '\é' AS escaped, '雪' AS ordinary;")
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        result.rows,
        vec![vec![
            Value::String("é".to_owned()),
            Value::String("雪".to_owned())
        ]]
    );
}

#[test]
fn ungrouped_columns_are_rejected_in_every_aggregate_clause() {
    let setup = "CREATE TABLE t (id Int64); INSERT INTO t VALUES (10), (20);";
    for query in [
        "SELECT id + count(*) FROM t;",
        "SELECT count(*) AS n FROM t HAVING id > 0;",
        "SELECT count(*) AS n FROM t ORDER BY id;",
    ] {
        let mut database = Database::new();
        database.execute(setup).unwrap();
        let error = database.execute(query).unwrap_err();
        assert!(
            matches!(error, Error::Execution(ref message) if message.contains("must appear in GROUP BY")),
            "query unexpectedly produced a different error: {query}: {error}"
        );
    }
}

#[test]
fn structurally_grouped_expressions_and_aggregate_aliases_are_valid() {
    let results = Database::new()
        .execute(
            "CREATE TABLE t (id Int64); INSERT INTO t VALUES (1), (1), (2);
             SELECT id + 1 AS key, count(*) AS n FROM t
             GROUP BY id + 1 HAVING n > 0 ORDER BY key;",
        )
        .unwrap();
    assert_eq!(
        results[0].rows,
        vec![
            vec![Value::Int64(2), Value::Int64(2)],
            vec![Value::Int64(3), Value::Int64(1)]
        ]
    );
}

#[test]
fn not_binds_to_comparisons_and_null_predicates_before_boolean_operators() {
    let result = Database::new()
        .execute(
            "CREATE TABLE t (id Int64, n Nullable(Int64));
             INSERT INTO t VALUES (1, NULL), (2, 3), (3, NULL);
             SELECT id FROM t
             WHERE NOT id = 1 AND NOT n IS NULL OR id = 3 ORDER BY id;",
        )
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        result.rows,
        vec![vec![Value::Int64(2)], vec![Value::Int64(3)]]
    );
}

#[test]
fn quoted_identifiers_remain_case_sensitive_and_are_never_keywords() {
    let mut database = Database::new();
    let result = database
        .execute(
            "CREATE TABLE \"Table\" (\"null\" Int64, \"Case\" String);
             INSERT INTO \"Table\" (\"null\", \"Case\") VALUES (7, 'ok');
             SELECT \"null\", \"Case\" FROM \"Table\";",
        )
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        result.rows,
        vec![vec![Value::Int64(7), Value::String("ok".to_owned())]]
    );
    assert!(matches!(
        database.execute("SELECT \"case\" FROM \"Table\";"),
        Err(Error::Execution(message)) if message.contains("unknown column")
    ));
}

#[test]
fn result_metadata_comes_from_schema_even_without_representative_values() {
    let results = Database::new()
        .execute(
            "CREATE TABLE t (id Int64, note Nullable(String));
             INSERT INTO t VALUES (1, 'present');
             SELECT id, note FROM t WHERE id < 0;
             SELECT note FROM t WHERE id = 1;",
        )
        .unwrap();
    assert_eq!(results[0].columns[0].data_type, Some(DataType::Int64));
    assert!(!results[0].columns[0].nullable);
    assert_eq!(results[0].columns[1].data_type, Some(DataType::String));
    assert!(results[0].columns[1].nullable);
    assert_eq!(results[1].columns[0].data_type, Some(DataType::String));
    assert!(results[1].columns[0].nullable);
}

#[test]
fn select_accepts_both_int64_boundaries() {
    let result = Database::new()
        .execute("SELECT -9223372036854775808 AS low, 9223372036854775807 AS high;")
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        result.rows,
        vec![vec![Value::Int64(i64::MIN), Value::Int64(i64::MAX)]]
    );
}

#[test]
fn deeply_nested_nullable_types_hit_a_typed_limit() {
    let sql = format!(
        "CREATE TABLE t (value {}Int64{});",
        "Nullable(".repeat(20_000),
        ")".repeat(20_000)
    );
    let error = Database::new().execute(&sql).unwrap_err();
    assert!(matches!(
        error,
        Error::Limit {
            resource: "SQL data type nesting",
            ..
        }
    ));
}

#[test]
fn empty_tables_do_not_hide_invalid_expression_types_or_functions() {
    let setup = "CREATE TABLE t (id Int64, s String);";
    for query in [
        "SELECT id + s FROM t;",
        "SELECT sum(s) FROM t;",
        "SELECT id FROM t WHERE mystery(id);",
        "SELECT count(*) FROM t GROUP BY mystery(id);",
        "SELECT id FROM t ORDER BY mystery(id);",
    ] {
        let mut database = Database::new();
        database.execute(setup).unwrap();
        let error = database.execute(query).unwrap_err();
        assert!(
            matches!(error, Error::Type(_) | Error::Execution(_)),
            "query unexpectedly produced a different error: {query}: {error}"
        );
    }
}

#[test]
fn qualified_columns_must_match_the_selected_table() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE t (id Int64); INSERT INTO t VALUES (7);")
        .unwrap();
    let valid = database.execute("SELECT t.id FROM t;").unwrap();
    assert_eq!(valid[0].rows, vec![vec![Value::Int64(7)]]);
    assert!(matches!(
        database.execute("SELECT bogus.id FROM t;"),
        Err(Error::Execution(message)) if message.contains("qualifier")
    ));
}

#[test]
fn composite_table_names_cannot_alias_quoted_dotted_names() {
    let results = Database::new()
        .execute(
            "CREATE TABLE \"a.q:b\" (id Int64);
             CREATE TABLE \"a\".\"b\" (id Int64);
             INSERT INTO \"a.q:b\" VALUES (1);
             INSERT INTO \"a\".\"b\" VALUES (2);
             SELECT id FROM \"a.q:b\";
             SELECT id FROM \"a\".\"b\";",
        )
        .unwrap();
    assert_eq!(results[0].rows, vec![vec![Value::Int64(1)]]);
    assert_eq!(results[1].rows, vec![vec![Value::Int64(2)]]);
}

#[test]
fn invalid_order_by_ordinals_are_rejected() {
    for ordinal in [0, 2] {
        let error = Database::new()
            .execute(&format!("SELECT 1 AS value ORDER BY {ordinal};"))
            .unwrap_err();
        assert!(
            matches!(error, Error::Execution(message) if message.contains("outside the projection"))
        );
    }
}

#[test]
fn explicit_aliases_resolve_consistently_when_columns_share_the_name() {
    let result = Database::new()
        .execute(
            "CREATE TABLE t (id Int64); INSERT INTO t VALUES (7);
             SELECT true AS id, id, count(*) AS n
             FROM t GROUP BY id HAVING id;",
        )
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        result.rows,
        vec![vec![Value::Bool(true), Value::Int64(7), Value::Int64(1)]]
    );
}

#[test]
fn quoting_preserves_case_without_creating_a_separate_namespace() {
    let mut database = Database::new();
    let result = database
        .execute(
            "CREATE TABLE \"foo\" (\"bar\" Int64);
             INSERT INTO foo (bar) VALUES (9);
             SELECT bar FROM foo;",
        )
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(result.rows, vec![vec![Value::Int64(9)]]);

    let error = database
        .execute("CREATE TABLE duplicate (id Int64, \"id\" String);")
        .unwrap_err();
    assert!(matches!(
        error,
        Error::Catalog(message) if message.contains("duplicate column")
    ));
}
