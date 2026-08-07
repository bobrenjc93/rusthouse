use rusthouse::batch::catalog::Catalog;
use rusthouse::batch::engine::{Database, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::sql::{Statement, parse};
use rusthouse::batch::storage::ColumnDef;
use rusthouse::batch::value::{DataType, Value};

#[test]
fn parses_exact_modifier_case_insensitively_without_changing_ordinary_create() {
    for (sql, name) in [
        ("CREATE TABLE IF NOT EXISTS events (id Int64)", "events"),
        ("create table if not exists Events (id int64);", "Events"),
        ("CrEaTe TaBlE If NoT ExIsTs EVENTS (id INT64)", "EVENTS"),
    ] {
        assert_eq!(
            parse(sql).expect("valid conditional create"),
            [Statement::CreateTableIfNotExists {
                name: name.to_owned(),
                columns: vec![ColumnDef {
                    name: "id".to_owned(),
                    data_type: DataType::Int64,
                }],
            }],
            "{sql:?}"
        );
    }

    assert_eq!(
        parse("CREATE TABLE events (id Int64)").expect("ordinary create remains valid"),
        [Statement::CreateTable {
            name: "events".to_owned(),
            columns: vec![ColumnDef {
                name: "id".to_owned(),
                data_type: DataType::Int64,
            }],
        }]
    );
    assert_eq!(
        parse("CREATE TABLE IF (id Int64)").expect("IF remains a legal table name"),
        [Statement::CreateTable {
            name: "IF".to_owned(),
            columns: vec![ColumnDef {
                name: "id".to_owned(),
                data_type: DataType::Int64,
            }],
        }]
    );
}

#[test]
fn catalog_no_op_keeps_the_existing_cap_instead_of_the_requested_cap() {
    let mut catalog = Catalog::new();
    catalog
        .create_table_with_row_cap(
            "Events".to_owned(),
            vec![ColumnDef {
                name: "id".to_owned(),
                data_type: DataType::Int64,
            }],
            2,
        )
        .expect("original table is valid");

    assert_eq!(
        catalog.create_table_if_not_exists_with_row_cap(
            "events".to_owned(),
            vec![ColumnDef {
                name: "replacement".to_owned(),
                data_type: DataType::String,
            }],
            99,
        ),
        Ok(false)
    );
    let original = catalog.table("EVENTS").expect("original table remains");
    assert_eq!(original.row_cap(), 2);
    assert_eq!(original.schema()[0].name, "id");
}

#[test]
fn rejects_malformed_if_not_exists_forms_as_typed_sql_errors() {
    for malformed in [
        "CREATE TABLE IF EXISTS events (id Int64)",
        "CREATE TABLE IF NOT events (id Int64)",
        "CREATE TABLE IF NOT EXIST events (id Int64)",
        "CREATE TABLE IF NOT EXISTS (id Int64)",
        "CREATE TABLE NOT EXISTS events (id Int64)",
        "CREATE TABLE IF NOT EXISTS events id Int64)",
    ] {
        assert!(
            matches!(parse(malformed), Err(Error::Sql { .. })),
            "malformed conditional create was accepted: {malformed:?}"
        );
    }
}

#[test]
fn existing_case_insensitive_match_is_a_no_op_that_preserves_all_table_state() {
    let mut database = Database::with_max_rows_per_table(2);
    database
        .execute(
            "CREATE TABLE EventLog (id Int64, label String); \
             INSERT INTO EventLog VALUES (1, 'original');",
        )
        .expect("setup succeeds");

    assert_eq!(
        database
            .execute("CREATE TABLE IF NOT EXISTS eventlog (replacement Bool);")
            .expect("case-insensitive match is suppressed"),
        [StatementResult::Command {
            tag: "CREATE TABLE",
            affected_rows: 0,
        }]
    );

    let table = database
        .catalog()
        .table("EVENTLOG")
        .expect("original table remains");
    assert_eq!(table.name(), "EventLog");
    assert_eq!(
        table.schema(),
        [
            ColumnDef {
                name: "id".to_owned(),
                data_type: DataType::Int64,
            },
            ColumnDef {
                name: "label".to_owned(),
                data_type: DataType::String,
            },
        ]
    );
    assert_eq!(table.row_count(), 1);
    assert_eq!(table.row_cap(), 2);

    database
        .execute("INSERT INTO EVENTLOG VALUES (2, 'retained');")
        .expect("the original remaining row capacity is retained");
    assert_eq!(
        database.execute("INSERT INTO EventLog VALUES (3, 'too many');"),
        Err(Error::ResourceLimitExceeded {
            resource: "table rows",
            actual: 3,
            max: 2,
        })
    );
    let results = database
        .execute("SELECT id, label FROM eventlog ORDER BY id;")
        .expect("the original schema remains queryable");
    let [StatementResult::Query(result)] = results.as_slice() else {
        panic!("SELECT must return one query result");
    };
    assert_eq!(
        result.rows,
        [
            vec![Value::Int64(1), Value::String("original".to_owned())],
            vec![Value::Int64(2), Value::String("retained".to_owned())],
        ]
    );
}

#[test]
fn absent_table_is_created_normally_and_plain_duplicate_still_errors() {
    let mut database = Database::new();
    assert_eq!(
        database
            .execute("CREATE TABLE IF NOT EXISTS metrics (value Float64);")
            .expect("absent table is created"),
        [StatementResult::Command {
            tag: "CREATE TABLE",
            affected_rows: 0,
        }]
    );
    assert_eq!(
        database
            .catalog()
            .table("METRICS")
            .expect("new table is registered")
            .schema(),
        [ColumnDef {
            name: "value".to_owned(),
            data_type: DataType::Float64,
        }]
    );
    assert_eq!(
        database.execute("CREATE TABLE metrics (different String);"),
        Err(Error::TableAlreadyExists("metrics".to_owned()))
    );
}
