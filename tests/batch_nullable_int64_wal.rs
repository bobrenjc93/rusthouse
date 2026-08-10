#![cfg(unix)]

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusthouse::batch::engine::{Database, StatementResult};
use rusthouse::batch::error::Error;
use rusthouse::batch::storage::Column;
use rusthouse::batch::value::{DataType, Value};
use rusthouse::batch::wal::{
    INT64_WAL_FRAME_HEADER_LEN, INT64_WAL_FRAME_OVERHEAD, INT64_WAL_VERSION,
    Int64WriteAheadLogCommitError, Int64WriteAheadLogCorruption, Int64WriteAheadLogError,
    Int64WriteAheadLogLimitError, Int64WriteAheadLogLimits, NULLABLE_INT64_WAL_VERSION,
};
use rusthouse::{
    DatabaseInt64WalEnableError, DatabaseInt64WalRecoveryError, Int64MinMaxIndexAdmission,
    Int64MinMaxIndexLimits, Int64RangePartition,
};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let base =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("target/batch-nullable-int64-wal-tests");
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

fn nullable_values(database: &Database, table_name: &str) -> Vec<Option<i64>> {
    let table = database.catalog().table(table_name).unwrap();
    assert_eq!(table.schema()[0].data_type, DataType::NullableInt64);
    let Column::NullableInt64(values) = &table.columns()[0] else {
        panic!("expected Nullable(Int64) physical column");
    };
    values.clone()
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

fn bootstrap_values_offset(bytes: &[u8]) -> usize {
    let payload = INT64_WAL_FRAME_HEADER_LEN;
    let table_len = usize::try_from(u64::from_le_bytes(
        bytes[payload..payload + 8].try_into().unwrap(),
    ))
    .unwrap();
    let column_len_field = payload + 8 + table_len;
    let column_len = usize::try_from(u64::from_le_bytes(
        bytes[column_len_field..column_len_field + 8]
            .try_into()
            .unwrap(),
    ))
    .unwrap();
    let column_end = column_len_field + 8 + column_len;
    column_end + 1 + 17 * 8 + 8
}

fn rewrite_frame_checksum(bytes: &mut [u8], frame: usize) {
    let version = u16::from_le_bytes(bytes[frame + 8..frame + 10].try_into().unwrap());
    let kind = bytes[frame + 10];
    let reserved = bytes[frame + 11];
    let sequence = u64::from_le_bytes(bytes[frame + 12..frame + 20].try_into().unwrap());
    let payload_len = usize::try_from(u64::from_le_bytes(
        bytes[frame + 20..frame + 28].try_into().unwrap(),
    ))
    .unwrap();
    let payload_start = frame + INT64_WAL_FRAME_HEADER_LEN;
    let payload = &bytes[payload_start..payload_start + payload_len];
    let mut checksum = u32::MAX;
    for byte in version
        .to_le_bytes()
        .into_iter()
        .chain([kind, reserved])
        .chain(sequence.to_le_bytes())
        .chain((payload_len as u64).to_le_bytes())
        .chain(payload.iter().copied())
    {
        checksum ^= u32::from(byte);
        for _ in 0..8 {
            let mask = 0_u32.wrapping_sub(checksum & 1);
            checksum = (checksum >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    bytes[frame + 28..frame + 32].copy_from_slice(&(!checksum).to_le_bytes());
}

#[test]
fn null_heavy_bootstrap_append_and_atomic_replacement_round_trip() {
    let directory = TestDirectory::new();
    let path = directory.join("nullable.wal");
    let limits = Int64WriteAheadLogLimits::default();
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE Readings (Measurement Nullable(Int64)); \
             INSERT INTO Readings VALUES (NULL), (1), (NULL), (-2), (NULL);",
        )
        .unwrap();
    assert!(matches!(
        database
            .create_int64_min_max_index(
                "readings",
                "measurement",
                Int64MinMaxIndexLimits::new(3, 3, usize::MAX),
            )
            .unwrap(),
        Int64MinMaxIndexAdmission::Created(_)
    ));
    database
        .enable_int64_write_ahead_log("readings", &path, limits)
        .unwrap();
    database
        .execute("INSERT INTO Readings VALUES (NULL), (9223372036854775807);")
        .unwrap();
    database
        .execute("ALTER TABLE Readings UPDATE Measurement = NULL WHERE Measurement = 1;")
        .unwrap();

    let mut recovered = Database::recover_int64_write_ahead_log(&path, limits).unwrap();
    assert_eq!(
        nullable_values(&recovered, "readings"),
        [None, None, None, Some(-2), None, None, Some(i64::MAX)]
    );
    let table = recovered.catalog().table("readings").unwrap();
    assert_eq!(table.name(), "Readings");
    assert_eq!(table.schema()[0].name, "Measurement");
    assert_eq!(table.retained_value_bytes(), 16);
    assert_eq!(table.int64_range_partition_count(), None);
    let aggregate_results = recovered
        .execute("SELECT COUNT(Measurement), MIN(Measurement), MAX(Measurement) FROM Readings")
        .unwrap();
    let [StatementResult::Query(aggregates)] = aggregate_results.as_slice() else {
        panic!("expected nullable aggregate result");
    };
    assert_eq!(
        aggregates.rows,
        [vec![
            Value::Int64(2),
            Value::Int64(-2),
            Value::Int64(i64::MAX)
        ]]
    );

    let blocks = database
        .catalog()
        .table("readings")
        .unwrap()
        .int64_min_max_index_blocks()
        .unwrap();
    assert_eq!(
        blocks.iter().map(|block| block.null_count).sum::<usize>(),
        5
    );
    let query_results = database
        .execute("SELECT Measurement FROM Readings WHERE Measurement >= -2 ORDER BY Measurement")
        .unwrap();
    let [StatementResult::Query(filtered)] = query_results.as_slice() else {
        panic!("expected one query result");
    };
    assert_eq!(
        filtered.rows,
        [vec![Value::Int64(-2)], vec![Value::Int64(i64::MAX)]]
    );

    database
        .execute("ALTER TABLE Readings UPDATE Measurement = 7 WHERE Measurement = -2;")
        .unwrap();
    let replaced = Database::recover_int64_write_ahead_log(&path, limits).unwrap();
    assert_eq!(
        nullable_values(&replaced, "readings"),
        [None, None, None, Some(7), None, None, Some(i64::MAX)]
    );
}

#[test]
fn nullable_version_is_tagged_while_non_nullable_wals_remain_version_one() {
    let directory = TestDirectory::new();
    let nullable_path = directory.join("nullable-version.wal");
    let plain_path = directory.join("plain-version.wal");

    let mut nullable = Database::new();
    nullable
        .execute("CREATE TABLE t (v Nullable(Int64)); INSERT INTO t VALUES (NULL), (0);")
        .unwrap();
    nullable
        .enable_int64_write_ahead_log("t", &nullable_path, Int64WriteAheadLogLimits::default())
        .unwrap();
    let nullable_bytes = fs::read(&nullable_path).unwrap();
    assert_eq!(
        u16::from_le_bytes(nullable_bytes[8..10].try_into().unwrap()),
        NULLABLE_INT64_WAL_VERSION
    );
    let values = bootstrap_values_offset(&nullable_bytes);
    assert_eq!(nullable_bytes[values], 0, "NULL has an explicit tag");
    assert_eq!(nullable_bytes[values + 1], 1, "a present value has a tag");
    assert_eq!(
        i64::from_le_bytes(nullable_bytes[values + 2..values + 10].try_into().unwrap()),
        0
    );

    let mut plain = Database::new();
    plain
        .execute("CREATE TABLE t (v Int64); INSERT INTO t VALUES (0);")
        .unwrap();
    plain
        .enable_int64_write_ahead_log("t", &plain_path, Int64WriteAheadLogLimits::default())
        .unwrap();
    assert!(matches!(
        plain.execute("ALTER TABLE t UPDATE v = NULL WHERE v = 0;"),
        Err(Error::TypeMismatch { ref actual, .. }) if actual == "NULL"
    ));
    let plain_bytes = fs::read(&plain_path).unwrap();
    assert_eq!(
        u16::from_le_bytes(plain_bytes[8..10].try_into().unwrap()),
        INT64_WAL_VERSION
    );
    let recovered =
        Database::recover_int64_write_ahead_log(&plain_path, Int64WriteAheadLogLimits::default())
            .unwrap();
    assert_eq!(
        recovered.catalog().table("t").unwrap().schema()[0].data_type,
        DataType::Int64
    );
}

#[test]
fn nullable_torn_tail_is_ignored_without_losing_bootstrap_null_positions() {
    let directory = TestDirectory::new();
    let path = directory.join("source.wal");
    let torn = directory.join("torn.wal");
    let limits = Int64WriteAheadLogLimits::default();
    let mut database = Database::new();
    database
        .execute("CREATE TABLE t (v Nullable(Int64)); INSERT INTO t VALUES (NULL), (4);")
        .unwrap();
    database
        .enable_int64_write_ahead_log("t", &path, limits)
        .unwrap();
    database
        .execute("INSERT INTO t VALUES (NULL), (5);")
        .unwrap();
    database.disable_int64_write_ahead_log();

    let bytes = fs::read(&path).unwrap();
    let append = *frame_starts(&bytes).last().unwrap();
    fs::write(&torn, &bytes[..append + INT64_WAL_FRAME_HEADER_LEN + 9]).unwrap();

    let recovered = Database::recover_int64_write_ahead_log(&torn, limits).unwrap();
    assert_eq!(nullable_values(&recovered, "t"), [None, Some(4)]);
}

#[test]
fn nullable_checksum_and_authenticated_tag_corruption_are_typed() {
    let directory = TestDirectory::new();
    let checksum_path = directory.join("checksum.wal");
    let tag_path = directory.join("tag.wal");
    let limits = Int64WriteAheadLogLimits::default();
    let mut database = Database::new();
    database
        .execute("CREATE TABLE t (v Nullable(Int64)); INSERT INTO t VALUES (NULL), (8);")
        .unwrap();
    database
        .enable_int64_write_ahead_log("t", &checksum_path, limits)
        .unwrap();
    database.disable_int64_write_ahead_log();

    let mut checksum_bytes = fs::read(&checksum_path).unwrap();
    let value_offset = bootstrap_values_offset(&checksum_bytes);
    checksum_bytes[value_offset] ^= 0x40;
    fs::write(&checksum_path, checksum_bytes).unwrap();
    assert!(matches!(
        Database::recover_int64_write_ahead_log(&checksum_path, limits),
        Err(DatabaseInt64WalRecoveryError::WriteAheadLog(
            Int64WriteAheadLogError::Corruption(Int64WriteAheadLogCorruption::Checksum {
                sequence: 0,
                ..
            })
        ))
    ));

    let mut tag_bytes = fs::read(directory.join("checksum.wal")).unwrap();
    tag_bytes[value_offset] = 0x7f;
    rewrite_frame_checksum(&mut tag_bytes, 0);
    fs::write(&tag_path, tag_bytes).unwrap();
    assert!(matches!(
        Database::recover_int64_write_ahead_log(&tag_path, limits),
        Err(DatabaseInt64WalRecoveryError::WriteAheadLog(
            Int64WriteAheadLogError::Corruption(Int64WriteAheadLogCorruption::MalformedPayload {
                sequence: 0,
                field: "row value"
            })
        ))
    ));
}

#[test]
fn empty_all_null_and_nullable_limits_are_atomic() {
    let directory = TestDirectory::new();
    let empty_path = directory.join("empty.wal");
    let all_null_path = directory.join("all-null.wal");
    let rejected_path = directory.join("rejected.wal");

    let mut empty = Database::new();
    empty
        .execute("CREATE TABLE empty (v Nullable(Int64));")
        .unwrap();
    empty
        .enable_int64_write_ahead_log("empty", &empty_path, Int64WriteAheadLogLimits::default())
        .unwrap();
    assert!(
        nullable_values(
            &Database::recover_int64_write_ahead_log(
                &empty_path,
                Int64WriteAheadLogLimits::default()
            )
            .unwrap(),
            "empty"
        )
        .is_empty()
    );

    let one_record = Int64WriteAheadLogLimits::new(64 * 1024, 16 * 1024, 1);
    let mut all_null = Database::new();
    all_null
        .execute("CREATE TABLE n (v Nullable(Int64)); INSERT INTO n VALUES (NULL), (NULL);")
        .unwrap();
    all_null
        .enable_int64_write_ahead_log("n", &all_null_path, one_record)
        .unwrap();
    assert_eq!(
        all_null.execute("INSERT INTO n VALUES (NULL);"),
        Err(Error::WriteAheadLog(Int64WriteAheadLogCommitError::Limit(
            Int64WriteAheadLogLimitError::Records {
                records: 2,
                max_records: 1,
            }
        )))
    );
    assert_eq!(nullable_values(&all_null, "n"), [None, None]);
    assert_eq!(
        nullable_values(
            &Database::recover_int64_write_ahead_log(&all_null_path, one_record).unwrap(),
            "n"
        ),
        [None, None]
    );

    let mut rejected = Database::new();
    rejected
        .execute("CREATE TABLE n (v Nullable(Int64)); INSERT INTO n VALUES (NULL);")
        .unwrap();
    assert!(matches!(
        rejected.enable_int64_write_ahead_log(
            "n",
            &rejected_path,
            Int64WriteAheadLogLimits::new(64 * 1024, 0, 10)
        ),
        Err(DatabaseInt64WalEnableError::WriteAheadLog(
            Int64WriteAheadLogError::Limit(Int64WriteAheadLogLimitError::RecordBytes {
                sequence: 0,
                ..
            })
        ))
    ));
    assert!(!rejected.int64_write_ahead_log_enabled());
    assert!(!rejected_path.exists());
}

#[test]
fn logged_append_preserves_existing_range_partition_invalidation() {
    let directory = TestDirectory::new();
    let path = directory.join("partitioned.wal");
    let limits = Int64WriteAheadLogLimits::default();
    let mut database = Database::new();
    database
        .create_int64_range_partitioned_table(
            "events",
            "id",
            vec![
                Int64RangePartition::new(-10, -1, vec![-10, -1]),
                Int64RangePartition::new(0, 10, vec![0, 10]),
            ],
        )
        .unwrap();
    assert_eq!(
        database
            .catalog()
            .table("events")
            .unwrap()
            .int64_range_partition_count(),
        Some(2)
    );
    database
        .enable_int64_write_ahead_log("events", &path, limits)
        .unwrap();
    database.execute("INSERT INTO events VALUES (11);").unwrap();
    assert_eq!(
        database
            .catalog()
            .table("events")
            .unwrap()
            .int64_range_partition_count(),
        None
    );
    let recovered = Database::recover_int64_write_ahead_log(&path, limits).unwrap();
    let Column::Int64(values) = &recovered.catalog().table("events").unwrap().columns()[0] else {
        panic!("expected non-nullable Int64 column");
    };
    assert_eq!(values, &[-10, -1, 0, 10, 11]);
}
