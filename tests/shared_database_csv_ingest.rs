use std::sync::mpsc;
use std::sync::{Arc, Barrier, RwLock, TryLockError};
use std::thread;
use std::time::Duration;

use rusthouse::batch::csv::{CsvIngestError, CsvIngestLimits};
use rusthouse::batch::engine::{Database, QueryResult, ResultColumn};
use rusthouse::batch::error::Error;
use rusthouse::batch::format::write_csv;
use rusthouse::batch::value::{DataType, Value};
use rusthouse::{SharedDatabase, SharedDatabaseError};

fn metrics_database(row_cap: usize) -> SharedDatabase {
    let database = SharedDatabase::with_max_rows_per_table(row_cap);
    database
        .execute("CREATE TABLE metrics (id Int64, score Float64, active Bool, label String);")
        .expect("create metrics table");
    database
}

#[test]
fn writer_output_round_trips_through_shared_database_at_exact_limits() {
    let expected = QueryResult {
        columns: vec![
            ResultColumn {
                name: "id".to_owned(),
                data_type: DataType::Int64,
            },
            ResultColumn {
                name: "score".to_owned(),
                data_type: DataType::Float64,
            },
            ResultColumn {
                name: "active".to_owned(),
                data_type: DataType::Bool,
            },
            ResultColumn {
                name: "label".to_owned(),
                data_type: DataType::String,
            },
        ],
        rows: vec![
            vec![
                Value::Int64(i64::MIN),
                Value::Float64(2.5),
                Value::Bool(true),
                Value::String("comma, quote \" and LF\nline".to_owned()),
            ],
            vec![
                Value::Int64(i64::MAX),
                Value::Float64(-0.125),
                Value::Bool(false),
                Value::String("CRLF\r\nline".to_owned()),
            ],
        ],
    };
    let mut csv = Vec::new();
    write_csv(&mut csv, &expected).expect("write CSVWithNames");
    let database = metrics_database(2);

    assert_eq!(
        database
            .ingest_csv_with_names("metrics", &csv, CsvIngestLimits::new(csv.len(), 2, 8),)
            .expect("ingest writer output at exact byte, row, value, and table limits"),
        2
    );
    assert_eq!(
        database
            .query("SELECT id, score, active, label FROM metrics ORDER BY id;")
            .expect("query imported rows"),
        expected
    );
}

#[test]
fn late_csv_error_is_typed_and_rolls_back_every_parsed_row() {
    let database = metrics_database(3);
    database
        .execute("INSERT INTO metrics VALUES (9, 9.0, true, 'existing');")
        .unwrap();
    let input = b"id,score,active,label\n1,1.5,true,valid\n2,NaN,false,late\n";

    assert_eq!(
        database.ingest_csv_with_names("metrics", input, CsvIngestLimits::new(input.len(), 2, 8),),
        Err(SharedDatabaseError::CsvIngest(
            CsvIngestError::InvalidValue {
                line: 3,
                column: 2,
                expected: DataType::Float64,
            }
        ))
    );
    assert_eq!(
        database
            .query("SELECT id, label FROM metrics;")
            .unwrap()
            .rows,
        [vec![Value::Int64(9), Value::String("existing".to_owned())]]
    );
}

struct BlockingInput {
    bytes: Vec<u8>,
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
}

impl AsRef<[u8]> for BlockingInput {
    fn as_ref(&self) -> &[u8] {
        self.entered.wait();
        self.release.wait();
        &self.bytes
    }
}

#[test]
fn csv_ingestion_holds_one_write_lock_through_input_access_and_atomic_append() {
    let mut initial = Database::with_max_rows_per_table(1);
    initial.execute("CREATE TABLE metrics (id Int64);").unwrap();
    let inner = Arc::new(RwLock::new(initial));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let input = BlockingInput {
        bytes: b"id\n1\n".to_vec(),
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
    };
    let ingest_database = database.clone();
    let ingest = thread::spawn(move || {
        ingest_database.ingest_csv_with_names("metrics", input, CsvIngestLimits::new(5, 1, 1))
    });

    entered.wait();
    assert!(matches!(inner.try_read(), Err(TryLockError::WouldBlock)));
    assert!(matches!(inner.try_write(), Err(TryLockError::WouldBlock)));

    let (sender, receiver) = mpsc::channel();
    let concurrent_database = database.clone();
    let concurrent = thread::spawn(move || {
        sender
            .send(concurrent_database.execute("INSERT INTO metrics VALUES (2);"))
            .unwrap();
    });
    assert!(
        receiver.recv_timeout(Duration::from_millis(100)).is_err(),
        "the concurrent writer must wait while CSV bytes are parsed"
    );

    release.wait();
    assert_eq!(ingest.join().unwrap(), Ok(1));
    assert_eq!(
        receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
        Err(SharedDatabaseError::Sql(Error::ResourceLimitExceeded {
            resource: "table rows",
            actual: 2,
            max: 1,
        }))
    );
    concurrent.join().unwrap();
    assert_eq!(
        database.query("SELECT id FROM metrics;").unwrap().rows,
        [vec![Value::Int64(1)]]
    );
}

#[test]
fn csv_ingestion_reports_lock_poisoning_before_accessing_input() {
    let inner = Arc::new(RwLock::new(Database::new()));
    let database = SharedDatabase::from_arc(Arc::clone(&inner));
    let poisoner = thread::spawn(move || {
        let _guard = inner.write().unwrap();
        panic!("poison database lock");
    });
    assert!(poisoner.join().is_err());

    let input = b"id\n1\n";
    assert_eq!(
        database.ingest_csv_with_names("metrics", input, CsvIngestLimits::new(input.len(), 1, 1),),
        Err(SharedDatabaseError::LockPoisoned)
    );
}
