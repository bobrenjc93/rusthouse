use std::fs;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rusthouse::formats::{CsvOptions, FormatError, NdjsonOptions};
use rusthouse::{
    ColumnDef, DataType, Database, Field, LimitKind, Schema, StatementResult, TransactionLimits,
    Value,
};

static NEXT_PATH: AtomicU64 = AtomicU64::new(0);

fn database_path() -> PathBuf {
    let sequence = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "rusthouse-durable-ingest-{}-{sequence}.db",
        std::process::id()
    ))
}

fn remove_database(path: &PathBuf) {
    let _ = fs::remove_file(path);
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".rusthouse-lock");
    let _ = fs::remove_file(PathBuf::from(lock));
}

fn rows(database: &Database, sql: &str) -> Vec<Vec<Value>> {
    match database.execute(sql).unwrap() {
        StatementResult::Query(result) => result.rows,
        result => panic!("expected query result, got {result:?}"),
    }
}

#[test]
fn sql_and_format_schemas_convert_without_losing_order_or_nullability() {
    let columns = vec![
        ColumnDef::new("id", DataType::Int64, false),
        ColumnDef::new("note", DataType::String, true),
    ];
    let schema = Schema::try_from(columns.as_slice()).unwrap();
    assert_eq!(
        schema.fields(),
        [
            Field::new("id", DataType::Int64, false),
            Field::new("note", DataType::String, true),
        ]
    );
    assert_eq!(schema.to_column_defs(), columns);
}

#[test]
fn csv_and_ndjson_imports_commit_atomically_and_survive_reopen() {
    let path = database_path();
    let database = Database::open(&path).unwrap();
    database
        .execute("CREATE TABLE events (id Int64, active Bool, note String NULL)")
        .unwrap();
    let mut session = database.session();

    let mut csv_options = CsvOptions::default();
    csv_options.limits.batch_rows = 1;
    assert_eq!(
        session
            .ingest_csv(
                "events",
                Cursor::new(b"id,active,note\n1,true,first\n2,false,\\N\n"),
                csv_options,
            )
            .unwrap(),
        2
    );

    let mut json_options = NdjsonOptions::default();
    json_options.limits.batch_rows = 1;
    assert_eq!(
        session
            .ingest_ndjson(
                "events",
                Cursor::new(
                    br#"{"note":"third","active":true,"id":3}
"#,
                ),
                json_options,
            )
            .unwrap(),
        1
    );
    drop(session);
    drop(database);

    let reopened = Database::open(&path).unwrap();
    assert_eq!(
        rows(&reopened, "SELECT * FROM events"),
        vec![
            vec![
                Value::Int64(1),
                Value::Bool(true),
                Value::String("first".into())
            ],
            vec![Value::Int64(2), Value::Bool(false), Value::Null],
            vec![
                Value::Int64(3),
                Value::Bool(true),
                Value::String("third".into())
            ],
        ]
    );
    drop(reopened);
    remove_database(&path);
}

#[test]
fn late_errors_do_not_change_autocommit_or_explicit_transaction_state() {
    let database = Database::new();
    database
        .execute("CREATE TABLE events (id Int64, note String)")
        .unwrap();
    let generation = database.current_generation().unwrap();

    let mut options = CsvOptions::default();
    options.limits.batch_rows = 1;
    let bad = b"id,note\n1,valid\nnot-an-integer,late\n";
    assert!(matches!(
        database
            .session()
            .ingest_csv("events", Cursor::new(bad), options.clone()),
        Err(FormatError::Conversion { row: 2, .. })
    ));
    assert_eq!(database.current_generation().unwrap(), generation);
    assert!(rows(&database, "SELECT * FROM events").is_empty());

    let mut session = database.session();
    session.begin().unwrap();
    session
        .execute("INSERT INTO events VALUES (9, 'already staged')")
        .unwrap();
    assert!(matches!(
        session.ingest_csv("events", Cursor::new(bad), options),
        Err(FormatError::Conversion { row: 2, .. })
    ));
    let staged = match session.execute("SELECT * FROM events").unwrap() {
        StatementResult::Query(result) => result.rows,
        _ => unreachable!(),
    };
    assert_eq!(
        staged,
        vec![vec![
            Value::Int64(9),
            Value::String("already staged".into())
        ]]
    );
    session.rollback().unwrap();
    assert!(rows(&database, "SELECT * FROM events").is_empty());
}

#[test]
fn transaction_limits_reject_the_whole_import() {
    let database = Database::with_limits(TransactionLimits::new(1, usize::MAX));
    database.execute("CREATE TABLE events (id Int64)").unwrap();
    let generation = database.current_generation().unwrap();
    let mut options = CsvOptions::default();
    options.limits.batch_rows = 1;

    assert!(matches!(
        database
            .session()
            .ingest_csv("events", Cursor::new(b"id\n1\n2\n"), options,),
        Err(FormatError::Database(
            rusthouse::Error::TransactionLimitExceeded {
                kind: LimitKind::Rows,
                limit: 1,
                attempted: 2,
            }
        ))
    ));
    assert_eq!(database.current_generation().unwrap(), generation);
    assert!(rows(&database, "SELECT * FROM events").is_empty());
}
