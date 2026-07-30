use rusthouse::catalog::RelationKind;
use rusthouse::{Database, Error, QueryResult, StatementResult, Value};

fn query(database: &mut Database, sql: &str) -> QueryResult {
    match database
        .execute(sql)
        .expect("query succeeds")
        .into_iter()
        .last()
        .expect("one result")
    {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

#[test]
fn chained_views_support_filters_and_nested_aggregation() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE sales (region String, amount Int64, active Bool);
             INSERT INTO sales VALUES
                ('west', 10, true), ('west', 5, true),
                ('east', 4, true), ('east', 100, false);
             CREATE VIEW active_sales AS
                SELECT region, amount FROM sales WHERE active = true;
             CREATE VIEW regional_totals AS
                SELECT region, SUM(amount) AS total
                FROM active_sales GROUP BY region;",
        )
        .expect("setup succeeds");

    let result = query(
        &mut database,
        "SELECT COUNT(*) AS regions, SUM(total) AS grand_total,
                AVG(total) AS mean_total
         FROM regional_totals;",
    );
    assert_eq!(
        result.rows,
        vec![vec![Value::Int64(2), Value::Int64(19), Value::Float64(9.5),]]
    );
}

#[test]
fn view_creation_checks_dependencies_without_executing_the_query() {
    let mut database = Database::new();

    let missing = database
        .execute("CREATE VIEW missing_view AS SELECT id FROM missing_table")
        .expect_err("missing dependency is rejected");
    assert!(matches!(missing, Error::TableNotFound(name) if name == "missing_table"));
    assert!(matches!(
        database.catalog().view("missing_view"),
        Err(Error::ViewNotFound(_))
    ));

    database
        .execute("CREATE TABLE empty_values (value Int64)")
        .expect("create table");
    database
        .execute("CREATE VIEW minimum_value AS SELECT MIN(value) AS minimum FROM empty_values")
        .expect("schema validation must not execute MIN over the empty table");

    let runtime = database
        .execute("SELECT minimum FROM minimum_value")
        .expect_err("MIN remains undefined when the view is queried");
    assert!(
        matches!(runtime, Error::InvalidQuery(message) if message.contains("MIN is undefined"))
    );
}

#[test]
fn aggregate_view_outputs_require_addressable_aliases() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE events (id Int64); INSERT INTO events VALUES (1), (2)")
        .expect("setup succeeds");

    let error = database
        .execute("CREATE VIEW counts AS SELECT COUNT(*) FROM events")
        .expect_err("generated aggregate name is not addressable");
    assert!(matches!(
        error,
        Error::InvalidQuery(message)
            if message.contains("output column 'COUNT(*)'")
                && message.contains("add an AS alias")
    ));
    assert!(matches!(
        database.catalog().view("counts"),
        Err(Error::ViewNotFound(_))
    ));

    database
        .execute("CREATE VIEW counts AS SELECT COUNT(*) AS count FROM events")
        .expect("aliased aggregate view succeeds");
    assert_eq!(
        query(&mut database, "SELECT count FROM counts").rows,
        vec![vec![Value::Int64(2)]]
    );
}

#[test]
fn dependent_views_revalidate_after_dependency_schema_changes() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE facts (old_value Int64, new_value String);
             INSERT INTO facts VALUES (7, 'seven');
             CREATE VIEW current_fact AS SELECT old_value FROM facts;
             CREATE VIEW exposed_fact AS SELECT * FROM current_fact;",
        )
        .expect("setup succeeds");

    assert_eq!(
        query(&mut database, "SELECT * FROM exposed_fact").rows,
        vec![vec![Value::Int64(7)]]
    );

    database
        .execute(
            "DROP VIEW current_fact;
             CREATE VIEW current_fact AS SELECT new_value FROM facts;",
        )
        .expect("replace dependency with a different schema");

    let expanded = query(&mut database, "SELECT * FROM exposed_fact");
    assert_eq!(expanded.columns[0].name, "new_value");
    assert_eq!(expanded.rows, vec![vec![Value::String("seven".to_owned())]]);

    database
        .execute("DROP VIEW current_fact")
        .expect("drop dependency");
    let missing = database
        .execute("SELECT * FROM exposed_fact")
        .expect_err("stored dependent view is revalidated");
    assert!(matches!(missing, Error::TableNotFound(name) if name == "current_fact"));
}

#[test]
fn views_have_catalog_identity_and_share_the_table_namespace() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE base (id Int64);
             CREATE VIEW Zebra AS SELECT id FROM base;
             CREATE VIEW alpha AS SELECT id FROM base;",
        )
        .expect("setup succeeds");

    assert_eq!(
        database.catalog().relation_kind("BASE"),
        Some(RelationKind::Table)
    );
    assert_eq!(
        database.catalog().relation_kind("zEbRa"),
        Some(RelationKind::View)
    );
    assert_eq!(
        database
            .catalog()
            .views()
            .map(|view| view.name())
            .collect::<Vec<_>>(),
        vec!["alpha", "Zebra"]
    );
    assert_eq!(
        database.catalog().view("ALPHA").unwrap().query().table,
        "base"
    );

    assert!(
        database
            .execute("CREATE VIEW base AS SELECT id FROM base")
            .is_err()
    );
    assert!(database.execute("CREATE TABLE zebra (id Int64)").is_err());
}

#[test]
fn drop_view_and_mutation_rules_preserve_physical_tables() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE source (id Int64);
             CREATE VIEW readonly_source AS SELECT id FROM source;",
        )
        .expect("setup succeeds");

    let mutation = database
        .execute("INSERT INTO readonly_source VALUES (1)")
        .expect_err("views are read-only");
    assert!(
        matches!(mutation, Error::InvalidQuery(message) if message.contains("cannot INSERT into view"))
    );

    database
        .execute("DROP VIEW IF EXISTS absent; DROP VIEW readonly_source")
        .expect("conditional and regular drops succeed");
    assert_eq!(database.catalog().relation_kind("readonly_source"), None);

    let missing = database
        .execute("DROP VIEW readonly_source")
        .expect_err("missing view needs IF EXISTS");
    assert!(matches!(missing, Error::ViewNotFound(_)));

    let wrong_kind = database
        .execute("DROP VIEW IF EXISTS source")
        .expect_err("IF EXISTS does not drop a table");
    assert!(matches!(wrong_kind, Error::InvalidQuery(message) if message.contains("not a view")));
    assert_eq!(
        database.catalog().relation_kind("source"),
        Some(RelationKind::Table)
    );
}

#[test]
fn cycles_are_rejected_and_failed_creation_is_rolled_back() {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE base (id Int64);
             CREATE VIEW first AS SELECT id FROM base;
             CREATE VIEW second AS SELECT id FROM first;
             DROP VIEW first;",
        )
        .expect("prepare a broken dependent view");

    let cycle = database
        .execute("CREATE VIEW first AS SELECT id FROM second")
        .expect_err("cycle is rejected");
    assert!(
        matches!(cycle, Error::InvalidQuery(message) if message.contains("first -> second -> first"))
    );
    assert!(matches!(
        database.catalog().view("first"),
        Err(Error::ViewNotFound(_))
    ));

    let direct = database
        .execute("CREATE VIEW self_reference AS SELECT id FROM self_reference")
        .expect_err("self-cycle is rejected");
    assert!(
        matches!(direct, Error::InvalidQuery(message) if message.contains("self_reference -> self_reference"))
    );
}

#[test]
fn view_expansion_depth_is_bounded() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE base (id Int64)")
        .expect("create base table");

    let mut source = "base".to_owned();
    for depth in 1..=rusthouse::engine::MAX_VIEW_EXPANSION_DEPTH {
        let view = format!("view_{depth}");
        database
            .execute(&format!("CREATE VIEW {view} AS SELECT id FROM {source}"))
            .unwrap_or_else(|error| panic!("depth {depth} should be accepted: {error}"));
        source = view;
    }

    let rejected = database
        .execute(&format!("CREATE VIEW too_deep AS SELECT id FROM {source}"))
        .expect_err("one more expansion is rejected");
    assert!(matches!(
        rejected,
        Error::InvalidQuery(message)
            if message.contains("view expansion exceeds maximum depth of 64")
    ));
    assert!(matches!(
        database.catalog().view("too_deep"),
        Err(Error::ViewNotFound(_))
    ));
}
