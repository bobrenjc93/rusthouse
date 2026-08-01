//! End-to-end logical-view tests at the public SQL boundary.

use rusthouse::{DataType, Database, Error, QueryResult, StatementResult, Value};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    let results = database.execute(sql).expect("query succeeds");
    match results.into_iter().last().expect("statement result") {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

#[test]
fn nested_aggregate_views_preserve_aliases_and_reflect_new_rows() {
    let mut database = Database::new();
    let setup = database
        .execute(
            "CREATE TABLE sales (region String, amount Int64, active Bool);
             INSERT INTO sales VALUES
                ('west', 10, true), ('east', 4, false), ('west', 7, true);
             CREATE VIEW active_sales AS
                SELECT region AS area, amount FROM sales WHERE active = true;
             CREATE VIEW regional_totals AS
                SELECT area, COUNT(*) AS orders, SUM(amount) AS total
                FROM active_sales GROUP BY area;",
        )
        .expect("view setup succeeds");
    assert!(matches!(
        setup.last(),
        Some(StatementResult::Command {
            tag: "CREATE VIEW",
            affected_rows: 0
        })
    ));

    let initial = query(
        &mut database,
        "SELECT area AS territory, orders, total
         FROM regional_totals ORDER BY total DESC",
    );
    assert_eq!(
        initial.columns,
        vec![
            rusthouse::ResultColumn {
                name: "territory".to_owned(),
                data_type: DataType::String,
            },
            rusthouse::ResultColumn {
                name: "orders".to_owned(),
                data_type: DataType::Int64,
            },
            rusthouse::ResultColumn {
                name: "total".to_owned(),
                data_type: DataType::Int64,
            },
        ]
    );
    assert_eq!(
        initial.rows,
        vec![vec![
            Value::String("west".to_owned()),
            Value::Int64(2),
            Value::Int64(17),
        ]]
    );

    database
        .execute("INSERT INTO sales VALUES ('east', 20, true), ('west', 1, false)")
        .expect("insert succeeds");
    let refreshed = query(
        &mut database,
        "SELECT * FROM regional_totals ORDER BY total DESC",
    );
    assert_eq!(
        refreshed.rows,
        vec![
            vec![
                Value::String("east".to_owned()),
                Value::Int64(1),
                Value::Int64(20),
            ],
            vec![
                Value::String("west".to_owned()),
                Value::Int64(2),
                Value::Int64(17),
            ],
        ]
    );
}

#[test]
fn creation_rejects_missing_and_invalid_dependencies() {
    let mut database = Database::new();
    let missing = database
        .execute("CREATE VIEW orphan AS SELECT id FROM absent")
        .expect_err("missing source is rejected");
    assert!(matches!(missing, Error::TableNotFound(name) if name == "absent"));
    assert!(matches!(
        database.catalog().view("orphan"),
        Err(Error::ViewNotFound(_))
    ));

    database
        .execute("CREATE TABLE items (id Int64, label String)")
        .expect("create table");
    let column = database
        .execute("CREATE VIEW invalid_column AS SELECT missing FROM items")
        .expect_err("invalid column is rejected at creation");
    assert!(matches!(column, Error::ColumnNotFound { column, .. } if column == "missing"));

    let aggregate = database
        .execute("CREATE VIEW invalid_sum AS SELECT SUM(label) AS total FROM items")
        .expect_err("invalid aggregate is rejected at creation");
    assert!(matches!(aggregate, Error::TypeMismatch { actual, .. } if actual == "String"));

    let aliases = database
        .execute("CREATE VIEW duplicate_names AS SELECT id, label AS id FROM items")
        .expect_err("view schema must have unique names");
    assert!(matches!(aliases, Error::DuplicateColumn(name) if name == "id"));
}

#[test]
fn views_are_read_only_and_drop_removes_only_the_view() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE events (id Int64);
             INSERT INTO events VALUES (1);
             CREATE VIEW visible_events AS SELECT id FROM events;",
        )
        .expect("setup succeeds");

    let write = database
        .execute("INSERT INTO visible_events VALUES (2)")
        .expect_err("views cannot be insert targets");
    assert!(matches!(write, Error::CannotModifyView(name) if name == "visible_events"));

    let dropped = database
        .execute("DROP VIEW visible_events")
        .expect("drop view succeeds");
    assert!(matches!(
        dropped.as_slice(),
        [StatementResult::Command {
            tag: "DROP VIEW",
            affected_rows: 0
        }]
    ));
    assert_eq!(
        query(&mut database, "SELECT COUNT(*) AS count FROM events").rows,
        vec![vec![Value::Int64(1)]]
    );
    let missing = database
        .execute("SELECT * FROM visible_events")
        .expect_err("dropped view is gone");
    assert!(matches!(missing, Error::TableNotFound(_)));
}

#[test]
fn recursive_dependencies_are_detected_without_catalog_mutation() {
    let mut database = Database::new();
    let direct = database
        .execute("CREATE VIEW self_ref AS SELECT * FROM self_ref")
        .expect_err("direct recursion is rejected");
    assert!(matches!(
        direct,
        Error::ViewDependencyCycle(path)
            if path == ["self_ref".to_owned(), "self_ref".to_owned()]
    ));

    database
        .execute(
            "CREATE TABLE seed (id Int64);
             CREATE VIEW first AS SELECT id FROM seed;
             CREATE VIEW second AS SELECT id FROM first;
             DROP VIEW first;",
        )
        .expect("prepare an unresolved dependency");
    let indirect = database
        .execute("CREATE VIEW first AS SELECT id FROM second")
        .expect_err("indirect recursion is rejected");
    assert!(matches!(indirect, Error::ViewDependencyCycle(path) if path.len() == 3));
    assert!(matches!(
        database.catalog().view("first"),
        Err(Error::ViewNotFound(_))
    ));
}

#[test]
fn referenced_views_revalidate_replaced_dependency_schemas() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE numbers (value Int64);
             CREATE TABLE labels (value String);
             CREATE VIEW current_values AS SELECT value FROM numbers;
             CREATE VIEW total AS SELECT SUM(value) AS amount FROM current_values;
             DROP VIEW current_values;
             CREATE VIEW current_values AS SELECT value FROM labels;",
        )
        .expect("replace a dependency with a different schema");

    let error = database
        .execute("SELECT * FROM total")
        .expect_err("dependent schema is revalidated when referenced");
    assert!(matches!(
        error,
        Error::TypeMismatch { expected, actual, .. }
            if expected == "Int64 or Float64" && actual == "String"
    ));
}

#[test]
fn view_expansion_has_a_hard_depth_limit() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE base (id Int64)")
        .expect("create base");

    let mut dependency = "base".to_owned();
    for index in 0..32 {
        let name = format!("v{index}");
        database
            .execute(&format!(
                "CREATE VIEW {name} AS SELECT id FROM {dependency}"
            ))
            .expect("view within depth limit");
        dependency = name;
    }
    let error = database
        .execute(&format!(
            "CREATE VIEW too_deep AS SELECT id FROM {dependency}"
        ))
        .expect_err("view beyond depth limit is rejected");
    assert_eq!(error, Error::ViewExpansionLimit { limit: 32 });
}
