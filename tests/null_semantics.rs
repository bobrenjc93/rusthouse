use rusthouse::format::{OutputFormat, render};
use rusthouse::storage::Column;
use rusthouse::{DataType, Database, Error, QueryResult, ResultColumn, StatementResult, Value};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("SQL succeeds");
    match results.into_iter().last().expect("statement result") {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

#[test]
fn nullable_columns_use_typed_values_and_validity_bitmaps_for_every_physical_type() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE nullable_values (
                i Nullable(Int64),
                f Nullable(Float64),
                b Nullable(Bool),
                s Nullable(String)
             );",
        )
        .expect("create table");

    let error = database
        .execute(
            "INSERT INTO nullable_values VALUES
                (NULL, NULL, NULL, NULL),
                (7, 2.5, true, false);",
        )
        .expect_err("the second row has the wrong String type");
    assert!(matches!(error, Error::TypeMismatch { .. }));
    assert_eq!(
        database
            .catalog()
            .table("nullable_values")
            .expect("table")
            .row_count(),
        0,
        "a multi-row insert is atomic"
    );

    database
        .execute(
            "INSERT INTO nullable_values VALUES
                (NULL, NULL, NULL, NULL),
                (7, 2.5, true, 'present');",
        )
        .expect("valid nullable rows");

    let table = database.catalog().table("nullable_values").expect("table");
    let expected_types = [
        DataType::Nullable(DataType::Int64),
        DataType::Nullable(DataType::Float64),
        DataType::Nullable(DataType::Bool),
        DataType::Nullable(DataType::String),
    ];
    for (column, expected_type) in table.columns().iter().zip(expected_types) {
        assert_eq!(column.data_type(), expected_type);
        let Column::Nullable { validity, .. } = column else {
            panic!("nullable schema uses bitmap-backed storage")
        };
        assert_eq!(validity.len(), 2);
        assert!(!validity.is_valid(0));
        assert!(validity.is_valid(1));
    }

    let Column::Nullable { values, .. } = &table.columns()[0] else {
        unreachable!()
    };
    assert!(matches!(values.as_ref(), Column::Int64(values) if values == &[0, 7]));
    let Column::Nullable { values, .. } = &table.columns()[1] else {
        unreachable!()
    };
    assert!(matches!(values.as_ref(), Column::Float64(values) if values == &[0.0, 2.5]));
    let Column::Nullable { values, .. } = &table.columns()[2] else {
        unreachable!()
    };
    assert!(matches!(values.as_ref(), Column::Bool(values) if values == &[false, true]));
    let Column::Nullable { values, .. } = &table.columns()[3] else {
        unreachable!()
    };
    assert!(matches!(values.as_ref(), Column::String(values) if values == &["", "present"]));

    assert_eq!(
        query(&mut database, "SELECT * FROM nullable_values;").rows,
        vec![
            vec![Value::Null, Value::Null, Value::Null, Value::Null],
            vec![
                Value::Int64(7),
                Value::Float64(2.5),
                Value::Bool(true),
                Value::String("present".to_owned()),
            ],
        ]
    );
}

#[test]
fn null_is_rejected_by_non_nullable_columns_without_partial_mutation() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE required_values (id Int64, note Nullable(String));")
        .expect("create table");

    let error = database
        .execute("INSERT INTO required_values VALUES (1, 'ok'), (NULL, NULL);")
        .expect_err("NULL cannot be inserted into Int64");
    assert!(matches!(
        error,
        Error::TypeMismatch { expected, actual, .. }
            if expected == "Int64" && actual == "NULL"
    ));
    assert_eq!(
        query(
            &mut database,
            "SELECT COUNT(*) AS rows FROM required_values;"
        )
        .rows,
        vec![vec![Value::Int64(0)]]
    );
}

#[test]
fn where_uses_sql_three_valued_truth_tables_and_is_null_tests() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE truth_table (id Int64, a Nullable(Int64), b Nullable(Int64));
             INSERT INTO truth_table VALUES
                (1, 1, 1), (2, 1, 0), (3, 1, NULL),
                (4, 0, 1), (5, 0, 0), (6, 0, NULL),
                (7, NULL, 1), (8, NULL, 0), (9, NULL, NULL);",
        )
        .expect("truth table setup");

    assert_eq!(
        query(
            &mut database,
            "SELECT id FROM truth_table WHERE a = 1 AND b = 1 ORDER BY id;"
        )
        .rows,
        vec![vec![Value::Int64(1)]]
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT id FROM truth_table WHERE a = 1 OR b = 1 ORDER BY id;"
        )
        .rows,
        [1, 2, 3, 4, 7]
            .into_iter()
            .map(|id| vec![Value::Int64(id)])
            .collect::<Vec<_>>()
    );
    assert!(
        query(
            &mut database,
            "SELECT id FROM truth_table WHERE a = NULL OR NULL = NULL;"
        )
        .rows
        .is_empty()
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT id FROM truth_table WHERE a IS NULL ORDER BY id;"
        )
        .rows,
        [7, 8, 9]
            .into_iter()
            .map(|id| vec![Value::Int64(id)])
            .collect::<Vec<_>>()
    );
    assert_eq!(
        query(
            &mut database,
            "SELECT id FROM truth_table WHERE a IS NOT NULL ORDER BY id;"
        )
        .rows
        .len(),
        6
    );
}

#[test]
fn nulls_form_groups_sort_deterministically_and_are_ignored_by_aggregates() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE grouped_nulls (category Nullable(String), amount Nullable(Int64));
             INSERT INTO grouped_nulls VALUES
                (NULL, NULL), (NULL, NULL),
                ('all-null', NULL), ('all-null', NULL),
                ('mixed', 1), ('mixed', NULL), ('mixed', 3);",
        )
        .expect("group setup");

    let grouped = query(
        &mut database,
        "SELECT category,
                COUNT(*) AS rows,
                COUNT(amount) AS present,
                SUM(amount) AS total,
                MIN(amount) AS low,
                MAX(amount) AS high,
                AVG(amount) AS mean
         FROM grouped_nulls
         GROUP BY category
         ORDER BY category;",
    );
    assert_eq!(
        grouped.rows,
        vec![
            vec![
                Value::String("all-null".to_owned()),
                Value::Int64(2),
                Value::Int64(0),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ],
            vec![
                Value::String("mixed".to_owned()),
                Value::Int64(3),
                Value::Int64(2),
                Value::Int64(4),
                Value::Int64(1),
                Value::Int64(3),
                Value::Float64(2.0),
            ],
            vec![
                Value::Null,
                Value::Int64(2),
                Value::Int64(0),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ],
        ]
    );

    assert_eq!(
        query(
            &mut database,
            "SELECT amount FROM grouped_nulls ORDER BY amount LIMIT 4;"
        )
        .rows,
        vec![
            vec![Value::Int64(1)],
            vec![Value::Int64(3)],
            vec![Value::Null],
            vec![Value::Null],
        ]
    );
    assert!(matches!(
        query(
            &mut database,
            "SELECT amount FROM grouped_nulls ORDER BY amount DESC LIMIT 1;"
        )
        .rows
        .as_slice(),
        [row] if row == &vec![Value::Null]
    ));
}

#[test]
fn empty_and_all_null_aggregates_return_null_for_every_supported_input() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE all_types (
                i Nullable(Int64), f Nullable(Float64),
                b Nullable(Bool), s Nullable(String)
             );
             INSERT INTO all_types VALUES (NULL, NULL, NULL, NULL), (NULL, NULL, NULL, NULL);",
        )
        .expect("all-null setup");

    let all_null = query(
        &mut database,
        "SELECT COUNT(*) AS rows, COUNT(i) AS present,
                SUM(i) AS sum_i, SUM(f) AS sum_f,
                MIN(i) AS min_i, MAX(f) AS max_f,
                MIN(b) AS min_b, MAX(b) AS max_b,
                MIN(s) AS min_s, MAX(s) AS max_s,
                AVG(i) AS avg_i, AVG(f) AS avg_f
         FROM all_types;",
    );
    assert_eq!(all_null.rows[0][0], Value::Int64(2));
    assert_eq!(all_null.rows[0][1], Value::Int64(0));
    assert!(
        all_null.rows[0][2..]
            .iter()
            .all(|value| *value == Value::Null)
    );

    let mut empty_database = Database::new();
    empty_database
        .execute("CREATE TABLE empty_values (n Int64);")
        .expect("empty table");
    assert_eq!(
        query(
            &mut empty_database,
            "SELECT COUNT(*) AS rows, SUM(n), MIN(n), MAX(n), AVG(n) FROM empty_values;"
        )
        .rows,
        vec![vec![
            Value::Int64(0),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ]]
    );
}

#[test]
fn null_is_rendered_in_table_csv_and_json_outputs() {
    let result = QueryResult {
        columns: vec![ResultColumn {
            name: "value".to_owned(),
            data_type: DataType::Nullable(DataType::String),
        }],
        rows: vec![vec![Value::Null]],
    };

    assert!(render(&result, OutputFormat::Table).contains("| NULL  |"));
    assert_eq!(render(&result, OutputFormat::Csv), "value\n\\N\n");
    assert_eq!(
        render(&result, OutputFormat::Json),
        r#"{"columns":[{"name":"value","type":"Nullable(String)"}],"rows":[[null]]}"#
    );
}
