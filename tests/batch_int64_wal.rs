#![cfg(unix)]

use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::num::NonZeroUsize;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use rusthouse::batch::engine::{Database, QueryResultLimits, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::storage::Column;
use rusthouse::batch::value::Value;
use rusthouse::batch::wal::{
    INT64_WAL_FRAME_HEADER_LEN, INT64_WAL_FRAME_OVERHEAD, Int64WriteAheadLogCommitError,
    Int64WriteAheadLogCorruption, Int64WriteAheadLogError, Int64WriteAheadLogLimitError,
    Int64WriteAheadLogLimits,
};
use rusthouse::{DatabaseInt64WalEnableError, DatabaseInt64WalRecoveryError, TableLimits};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/batch-int64-wal-tests");
        fs::create_dir_all(&base).unwrap();
        loop {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = base.join(format!("{}-{sequence}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("could not create test directory: {error}"),
            }
        }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn int64_values(database: &Database, table_name: &str) -> Vec<i64> {
    let table = database.catalog().table(table_name).unwrap();
    let Column::Int64(values) = &table.columns()[0] else {
        panic!("expected Int64 column");
    };
    values.clone()
}

fn nullable_int64_values(database: &Database, table_name: &str) -> Vec<Option<i64>> {
    let table = database.catalog().table(table_name).unwrap();
    let Column::NullableInt64(values) = &table.columns()[0] else {
        panic!("expected nullable Int64 column");
    };
    values.clone()
}

fn nullable_int64_minimum(database: &mut Database, table_name: &str) -> Value {
    let results = database
        .execute(&format!("SELECT MIN(Measurement) FROM {table_name}"))
        .unwrap();
    let [StatementResult::Query(result)] = results.as_slice() else {
        panic!("expected one MIN query result")
    };
    result.rows[0][0].clone()
}

fn metrics(database: &mut Database) -> Vec<Vec<Value>> {
    let results = database
        .execute("SELECT metric, value FROM system.metrics")
        .unwrap();
    let [StatementResult::Query(result)] = results.as_slice() else {
        panic!("expected metrics query");
    };
    result.rows.clone()
}

fn frame_starts(bytes: &[u8]) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut offset = 0;
    while bytes.len().saturating_sub(offset) >= INT64_WAL_FRAME_HEADER_LEN {
        starts.push(offset);
        let payload_len = u64::from_le_bytes(bytes[offset + 20..offset + 28].try_into().unwrap());
        offset += INT64_WAL_FRAME_OVERHEAD + usize::try_from(payload_len).unwrap();
        if offset > bytes.len() {
            break;
        }
    }
    starts
}

#[test]
fn clean_replay_preserves_names_caps_settings_and_cached_metrics() {
    let directory = TestDirectory::new();
    let path = directory.join("readings.wal");
    let mut database = Database::with_table_limits(TableLimits::new(5, 1, 5));
    database.set_global_aggregate_worker_cap(NonZeroUsize::new(2).unwrap());
    database
        .execute(
            "CREATE TABLE Readings (Measurement Int64); \
             INSERT INTO Readings VALUES (-1), (2);",
        )
        .unwrap();
    database
        .enable_int64_write_ahead_log("readings", &path, Int64WriteAheadLogLimits::default())
        .unwrap();
    database
        .execute("INSERT INTO READINGS VALUES (3);")
        .unwrap();
    let null_error = database
        .execute("INSERT INTO READINGS VALUES (NULL);")
        .unwrap_err();
    assert_eq!(
        null_error,
        Error::TypeMismatch {
            context: "column 'Readings.Measurement'".to_owned(),
            expected: "Int64".to_owned(),
            actual: "NULL".to_owned(),
        }
    );

    let mut recovered =
        Database::recover_int64_write_ahead_log(&path, Int64WriteAheadLogLimits::default())
            .unwrap();

    let table = recovered.catalog().table("READINGS").unwrap();
    assert_eq!(table.name(), "Readings");
    assert_eq!(table.schema()[0].name, "Measurement");
    assert_eq!(table.limits(), TableLimits::new(5, 1, 5));
    assert_eq!(recovered.table_limits(), TableLimits::new(5, 1, 5));
    assert_eq!(
        recovered.query_result_limits(),
        QueryResultLimits::default()
    );
    assert_eq!(recovered.global_aggregate_worker_cap().get(), 2);
    assert_eq!(int64_values(&recovered, "readings"), [-1, 2, 3]);
    assert_eq!(
        metrics(&mut recovered),
        [
            vec![
                Value::String("rusthouse_tables".to_owned()),
                Value::Int64(1)
            ],
            vec![
                Value::String("rusthouse_columns".to_owned()),
                Value::Int64(1)
            ],
            vec![
                Value::String("rusthouse_retained_rows".to_owned()),
                Value::Int64(3)
            ],
            vec![
                Value::String("rusthouse_retained_value_bytes".to_owned()),
                Value::Int64(24)
            ],
            vec![
                Value::String("rusthouse_index_scanned_blocks".to_owned()),
                Value::Int64(0)
            ],
            vec![
                Value::String("rusthouse_index_pruned_blocks".to_owned()),
                Value::Int64(0)
            ],
        ]
    );
    assert!(!recovered.int64_write_ahead_log_enabled());
}

#[test]
fn sql_null_append_replays_for_nullable_int64_wal() {
    let directory = TestDirectory::new();
    let path = directory.join("nullable-sql.wal");
    let limits = Int64WriteAheadLogLimits::default();
    let mut database = Database::new();
    database
        .create_nullable_int64_table("Readings", "Measurement", vec![Some(i64::MAX)])
        .unwrap();
    database
        .enable_int64_write_ahead_log("readings", &path, limits)
        .unwrap();

    database
        .execute(
            "INSERT INTO READINGS (MEASUREMENT) VALUES \
             (NULL), (-9223372036854775808), (nUlL);",
        )
        .unwrap();
    assert_eq!(
        nullable_int64_values(&database, "readings"),
        [Some(i64::MAX), None, Some(i64::MIN), None]
    );
    assert_eq!(
        nullable_int64_minimum(&mut database, "readings"),
        Value::Int64(i64::MIN)
    );

    let mut recovered = Database::recover_int64_write_ahead_log(&path, limits).unwrap();
    assert_eq!(
        nullable_int64_values(&recovered, "READINGS"),
        [Some(i64::MAX), None, Some(i64::MIN), None]
    );
    assert_eq!(
        nullable_int64_minimum(&mut recovered, "READINGS"),
        Value::Int64(i64::MIN)
    );
    let results = recovered
        .execute("SELECT toString(Measurement) AS rendered FROM READINGS ORDER BY rendered")
        .unwrap();
    let [StatementResult::Query(rendered)] = results.as_slice() else {
        panic!("expected toString query")
    };
    assert_eq!(
        rendered.rows,
        [
            vec![Value::Null(rusthouse::batch::value::DataType::String)],
            vec![Value::Null(rusthouse::batch::value::DataType::String)],
            vec![Value::String("-9223372036854775808".to_owned())],
            vec![Value::String("9223372036854775807".to_owned())],
        ]
    );

    database
        .replace_nullable_int64_values("readings", &[(0, None), (2, None)])
        .unwrap();
    let mut all_null_recovered = Database::recover_int64_write_ahead_log(&path, limits).unwrap();
    assert_eq!(
        nullable_int64_values(&all_null_recovered, "readings"),
        [None, None, None, None]
    );
    assert_eq!(
        nullable_int64_minimum(&mut all_null_recovered, "readings"),
        Value::Null(rusthouse::batch::value::DataType::Int64)
    );
    let results = all_null_recovered
        .execute("SELECT toString(Measurement) FROM readings")
        .unwrap();
    let [StatementResult::Query(rendered)] = results.as_slice() else {
        panic!("expected all-NULL toString query")
    };
    assert_eq!(
        rendered.rows,
        vec![vec![Value::Null(rusthouse::batch::value::DataType::String)]; 4]
    );
}

#[test]
fn failed_sql_null_wal_append_is_not_published_to_nullable_storage() {
    let directory = TestDirectory::new();
    let path = directory.join("bounded-nullable-sql.wal");
    let limits = Int64WriteAheadLogLimits::new(64 * 1024, 16 * 1024, 1);
    let mut database = Database::new();
    database
        .create_nullable_int64_table("Readings", "Measurement", Vec::new())
        .unwrap();
    database
        .enable_int64_write_ahead_log("readings", &path, limits)
        .unwrap();

    assert!(matches!(
        database.execute("INSERT INTO readings VALUES (NULL);"),
        Err(Error::WriteAheadLog(Int64WriteAheadLogCommitError::Limit(
            Int64WriteAheadLogLimitError::Records {
                records: 2,
                max_records: 1,
            }
        )))
    ));
    assert!(nullable_int64_values(&database, "readings").is_empty());

    let mut recovered = Database::recover_int64_write_ahead_log(&path, limits).unwrap();
    assert!(nullable_int64_values(&recovered, "READINGS").is_empty());
    assert_eq!(
        nullable_int64_minimum(&mut recovered, "READINGS"),
        Value::Null(rusthouse::batch::value::DataType::Int64)
    );
}

#[test]
fn replay_preserves_custom_query_row_byte_and_working_state_caps() {
    let directory = TestDirectory::new();
    let path = directory.join("query-limits.wal");
    let query_limits = QueryResultLimits {
        max_scan_rows: 91,
        max_rows: 81,
        max_values: 71,
        max_bytes: 61,
        max_ordering_state_bytes: 51,
        max_groups: 41,
        max_group_key_cells: 31,
        max_group_key_bytes: 21,
        max_aggregate_state_cells: 11,
        max_aggregate_state_bytes: 1,
    };
    let mut database = Database::with_query_result_limits(query_limits);
    database
        .execute("CREATE TABLE Caps (Value Int64);")
        .unwrap();
    database
        .enable_int64_write_ahead_log("caps", &path, Int64WriteAheadLogLimits::default())
        .unwrap();

    let recovered =
        Database::recover_int64_write_ahead_log(&path, Int64WriteAheadLogLimits::default())
            .unwrap();
    assert_eq!(recovered.query_result_limits(), query_limits);
}

#[test]
fn replays_append_atomic_replacement_and_truncate_and_is_repeatable() {
    let directory = TestDirectory::new();
    let path = directory.join("mutations.wal");
    let limits = Int64WriteAheadLogLimits::default();
    let mut database = Database::with_max_rows_per_table(5);
    database
        .execute("CREATE TABLE Events (Id Int64); INSERT INTO Events VALUES (1), (2);")
        .unwrap();
    database
        .enable_int64_write_ahead_log("events", &path, limits)
        .unwrap();

    database.execute("INSERT INTO EVENTS VALUES (3);").unwrap();
    let after_append = Database::recover_int64_write_ahead_log(&path, limits).unwrap();
    assert_eq!(int64_values(&after_append, "events"), [1, 2, 3]);

    database
        .execute("ALTER TABLE Events UPDATE Id = 20 WHERE Id = 2;")
        .unwrap();
    let after_replace = Database::recover_int64_write_ahead_log(&path, limits).unwrap();
    assert_eq!(int64_values(&after_replace, "events"), [1, 20, 3]);

    database.execute("TRUNCATE TABLE events;").unwrap();
    let after_truncate = Database::recover_int64_write_ahead_log(&path, limits).unwrap();
    let repeated = Database::recover_int64_write_ahead_log(&path, limits).unwrap();
    assert!(int64_values(&after_truncate, "events").is_empty());
    assert_eq!(
        int64_values(&repeated, "events"),
        int64_values(&after_truncate, "events")
    );
}

#[test]
fn ignores_torn_final_headers_bodies_and_commit_footers() {
    let directory = TestDirectory::new();
    let path = directory.join("torn.wal");
    let limits = Int64WriteAheadLogLimits::default();
    let mut database = Database::new();
    database
        .execute("CREATE TABLE events (id Int64); INSERT INTO events VALUES (1);")
        .unwrap();
    database
        .enable_int64_write_ahead_log("events", &path, limits)
        .unwrap();
    database.execute("INSERT INTO events VALUES (2);").unwrap();
    database.execute("INSERT INTO events VALUES (3);").unwrap();
    database.disable_int64_write_ahead_log();

    let complete = fs::read(&path).unwrap();
    let last_record = *frame_starts(&complete).last().unwrap();
    let payload_len = usize::try_from(u64::from_le_bytes(
        complete[last_record + 20..last_record + 28]
            .try_into()
            .unwrap(),
    ))
    .unwrap();
    let body_end = last_record + INT64_WAL_FRAME_HEADER_LEN + payload_len;

    for (phase, length) in [
        ("header", last_record + 5),
        ("body", last_record + INT64_WAL_FRAME_HEADER_LEN + 12),
        ("footer", body_end + 7),
    ] {
        let torn_path = directory.join(&format!("torn-{phase}.wal"));
        fs::write(&torn_path, &complete[..length]).unwrap();
        let recovered = Database::recover_int64_write_ahead_log(&torn_path, limits).unwrap();
        assert_eq!(int64_values(&recovered, "events"), [1, 2], "{phase}");
    }
}

#[test]
fn complete_checksum_corruption_is_typed_and_returns_no_database() {
    let directory = TestDirectory::new();
    let path = directory.join("corrupt.wal");
    let limits = Int64WriteAheadLogLimits::default();
    let mut source = Database::new();
    source.execute("CREATE TABLE events (id Int64);").unwrap();
    source
        .enable_int64_write_ahead_log("events", &path, limits)
        .unwrap();
    source.execute("INSERT INTO events VALUES (7);").unwrap();
    source.disable_int64_write_ahead_log();
    let source_values = int64_values(&source, "events");

    let mut bytes = fs::read(&path).unwrap();
    let starts = frame_starts(&bytes);
    let append = starts[1];
    bytes[append + INT64_WAL_FRAME_HEADER_LEN + 8] ^= 0x40;
    fs::write(&path, bytes).unwrap();

    assert!(matches!(
        Database::recover_int64_write_ahead_log(&path, limits),
        Err(DatabaseInt64WalRecoveryError::WriteAheadLog(
            Int64WriteAheadLogError::Corruption(Int64WriteAheadLogCorruption::Checksum {
                sequence: 1,
                ..
            })
        ))
    ));
    assert_eq!(int64_values(&source, "events"), source_values);
}

#[test]
fn committed_final_record_with_increased_payload_length_is_corruption() {
    let directory = TestDirectory::new();
    let path = directory.join("length-corrupt.wal");
    let limits = Int64WriteAheadLogLimits::default();
    let mut source = Database::new();
    source.execute("CREATE TABLE events (id Int64);").unwrap();
    source
        .enable_int64_write_ahead_log("events", &path, limits)
        .unwrap();
    source.execute("INSERT INTO events VALUES (7);").unwrap();
    source.disable_int64_write_ahead_log();

    let mut bytes = fs::read(&path).unwrap();
    let append = *frame_starts(&bytes).last().unwrap();
    let payload_len = u64::from_le_bytes(bytes[append + 20..append + 28].try_into().unwrap());
    bytes[append + 20..append + 28].copy_from_slice(&(payload_len + 1).to_le_bytes());
    fs::write(&path, bytes).unwrap();

    assert!(matches!(
        Database::recover_int64_write_ahead_log(&path, limits),
        Err(DatabaseInt64WalRecoveryError::WriteAheadLog(
            Int64WriteAheadLogError::Corruption(Int64WriteAheadLogCorruption::PayloadLength {
                sequence: 1,
                declared,
                committed,
            })
        )) if declared == payload_len + 1 && committed == payload_len
    ));
}

#[test]
fn committed_intermediate_record_with_overlong_payload_length_is_corruption() {
    let directory = TestDirectory::new();
    let path = directory.join("intermediate-length-corrupt.wal");
    let limits = Int64WriteAheadLogLimits::default();
    let mut source = Database::new();
    source.execute("CREATE TABLE events (id Int64);").unwrap();
    source
        .enable_int64_write_ahead_log("events", &path, limits)
        .unwrap();
    source.execute("INSERT INTO events VALUES (7);").unwrap();
    source.execute("INSERT INTO events VALUES (8);").unwrap();
    source.disable_int64_write_ahead_log();

    let mut bytes = fs::read(&path).unwrap();
    let starts = frame_starts(&bytes);
    assert_eq!(starts.len(), 3, "expected bootstrap and two appends");
    let append_seven = starts[1];
    let committed_payload_len = u64::from_le_bytes(
        bytes[append_seven + 20..append_seven + 28]
            .try_into()
            .unwrap(),
    );
    let declared_payload_len =
        u64::try_from(bytes.len() - append_seven - INT64_WAL_FRAME_OVERHEAD + 1).unwrap();
    assert!(declared_payload_len > committed_payload_len);
    assert!(declared_payload_len <= limits.max_record_bytes as u64);
    bytes[append_seven + 20..append_seven + 28]
        .copy_from_slice(&declared_payload_len.to_le_bytes());
    fs::write(&path, bytes).unwrap();

    assert!(matches!(
        Database::recover_int64_write_ahead_log(&path, limits),
        Err(DatabaseInt64WalRecoveryError::WriteAheadLog(
            Int64WriteAheadLogError::Corruption(Int64WriteAheadLogCorruption::PayloadLength {
                sequence: 1,
                declared,
                committed,
            })
        )) if declared == declared_payload_len && committed == committed_payload_len
    ));
}

#[test]
fn recovery_rejects_a_fifo_without_waiting_for_a_writer() {
    let directory = TestDirectory::new();
    let path = directory.join("wal.fifo");
    let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: `c_path` is a NUL-terminated pathname in this test's directory.
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);

    let recovery_path = path.clone();
    let (sender, receiver) = mpsc::channel();
    let recovery = thread::spawn(move || {
        let rejected = matches!(
            Database::recover_int64_write_ahead_log(
                &recovery_path,
                Int64WriteAheadLogLimits::default(),
            ),
            Err(DatabaseInt64WalRecoveryError::WriteAheadLog(
                Int64WriteAheadLogError::NotRegularFile
            ))
        );
        sender.send(rejected).unwrap();
    });

    match receiver.recv_timeout(Duration::from_secs(1)) {
        Ok(rejected) => assert!(rejected, "FIFO returned an unexpected recovery result"),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // Release a regressed blocking read-open so this test can fail
            // without leaving a permanently blocked test process behind.
            let _writer = OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&path)
                .expect("blocked FIFO recovery did not expose a waiting reader");
            let _ = receiver.recv_timeout(Duration::from_secs(1));
            recovery.join().unwrap();
            panic!("FIFO recovery blocked while waiting for a writer");
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            recovery.join().unwrap();
            panic!("FIFO recovery thread disconnected")
        }
    }
    recovery.join().unwrap();
}

#[test]
fn record_and_recovery_limits_are_typed_and_failed_publish_is_atomic() {
    let directory = TestDirectory::new();
    let path = directory.join("bounded.wal");
    let limits = Int64WriteAheadLogLimits::new(64 * 1024, 16 * 1024, 1);
    let mut database = Database::with_max_rows_per_table(2);
    database
        .execute("CREATE TABLE events (id Int64); INSERT INTO events VALUES (1);")
        .unwrap();
    database
        .enable_int64_write_ahead_log("events", &path, limits)
        .unwrap();

    assert!(matches!(
        database.execute("INSERT INTO events VALUES (2);"),
        Err(Error::WriteAheadLog(Int64WriteAheadLogCommitError::Limit(
            Int64WriteAheadLogLimitError::Records {
                records: 2,
                max_records: 1,
            }
        )))
    ));
    assert_eq!(int64_values(&database, "events"), [1]);

    assert!(matches!(
        Database::recover_int64_write_ahead_log(
            &path,
            Int64WriteAheadLogLimits::new(64 * 1024, 16 * 1024, 0),
        ),
        Err(DatabaseInt64WalRecoveryError::WriteAheadLog(
            Int64WriteAheadLogError::Limit(Int64WriteAheadLogLimitError::Records {
                records: 1,
                max_records: 0,
            })
        ))
    ));

    let recovered = Database::recover_int64_write_ahead_log(&path, limits).unwrap();
    assert_eq!(recovered.max_rows_per_table(), 2);
    assert_eq!(int64_values(&recovered, "events"), [1]);
}

#[test]
fn enable_preflights_nullable_bootstrap_limits_before_materializing_wal_values() {
    let directory = TestDirectory::new();
    let path = directory.join("too-small.wal");
    let mut database = Database::new();
    database
        .create_nullable_int64_table("Events", "id", vec![Some(7); 100_000])
        .unwrap();
    let limits = Int64WriteAheadLogLimits::new(usize::MAX, 1, usize::MAX);

    assert!(matches!(
        database.enable_int64_write_ahead_log("Events", &path, limits),
        Err(DatabaseInt64WalEnableError::WriteAheadLog(
            Int64WriteAheadLogError::Limit(Int64WriteAheadLogLimitError::RecordBytes {
                sequence: 0,
                max_bytes: 1,
                ..
            })
        ))
    ));
    assert!(!path.exists());
    assert!(!database.int64_write_ahead_log_enabled());
}

#[test]
fn unix_creation_syncs_a_complete_file_and_parent_before_enabling() {
    let directory = TestDirectory::new();
    let path = directory.join("durable.wal");
    let limits = Int64WriteAheadLogLimits::default();
    let mut database = Database::new();
    database.execute("CREATE TABLE events (id Int64);").unwrap();
    database
        .enable_int64_write_ahead_log("events", &path, limits)
        .unwrap();

    assert!(path.is_file());
    assert!(database.int64_write_ahead_log_enabled());
    assert!(Database::recover_int64_write_ahead_log(&path, limits).is_ok());

    let mut second = Database::new();
    second.execute("CREATE TABLE events (id Int64);").unwrap();
    assert!(matches!(
        second.enable_int64_write_ahead_log("events", &path, limits),
        Err(DatabaseInt64WalEnableError::WriteAheadLog(
            Int64WriteAheadLogError::Create(error)
        )) if error.kind() == ErrorKind::AlreadyExists
    ));

    let missing_parent = directory.join("missing").join("wal");
    assert!(matches!(
        second.enable_int64_write_ahead_log("events", &missing_parent, limits),
        Err(DatabaseInt64WalEnableError::WriteAheadLog(
            Int64WriteAheadLogError::OpenParent(error)
        )) if error.kind() == ErrorKind::NotFound
    ));
    assert!(!missing_parent.exists());
}
