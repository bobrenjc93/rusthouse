#![cfg(unix)]

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusthouse::batch::engine::{Database, StatementResult};
use rusthouse::batch::storage::{Column, Int64RangePartition, Int64RangePartitionLimits};
use rusthouse::batch::value::Value;
use rusthouse::batch::wal::{
    Int64WriteAheadLogError, Int64WriteAheadLogLimits, Int64WriteAheadLogRegistryCorruption,
    Int64WriteAheadLogRegistryError, Int64WriteAheadLogRegistryLimitError,
    Int64WriteAheadLogRegistryLimits,
};
use rusthouse::{
    DatabaseInt64WalRegistryEnableError, DatabaseInt64WalRegistryRecoveryError, TableLimits,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/wal-registry-tests");
        fs::create_dir_all(&base).unwrap();
        loop {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
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

fn limits() -> Int64WriteAheadLogRegistryLimits {
    Int64WriteAheadLogRegistryLimits::new(
        8,
        16 * 1024,
        1024 * 1024,
        128,
        Int64WriteAheadLogLimits::new(128 * 1024, 32 * 1024, 64),
    )
}

fn non_nullable(database: &Database, table: &str) -> Vec<i64> {
    let Column::Int64(values) = &database.catalog().table(table).unwrap().columns()[0] else {
        panic!("expected non-nullable Int64")
    };
    values.clone()
}

fn nullable(database: &Database, table: &str) -> Vec<Option<i64>> {
    let Column::NullableInt64(values) = &database.catalog().table(table).unwrap().columns()[0]
    else {
        panic!("expected nullable Int64")
    };
    values.clone()
}

fn create_two_table_registry(path: &Path) {
    let mut database = Database::with_table_limits(TableLimits::new(8, 1, 8));
    database
        .execute("CREATE TABLE Beta (v Int64); INSERT INTO Beta VALUES (2);")
        .unwrap();
    database
        .create_nullable_int64_table("Alpha", "v", vec![Some(1), None])
        .unwrap();
    database
        .enable_int64_write_ahead_log_registry(&["Beta", "Alpha"], path, limits())
        .unwrap();
    database.disable_int64_write_ahead_log();
}

#[test]
fn mixed_tables_mutate_restart_deterministically_and_invalidate_pruning_metadata() {
    let directory = TestDirectory::new();
    let registry = directory.join("registry");
    let mut database = Database::with_table_limits(TableLimits::new(12, 1, 12));
    database
        .execute("CREATE TABLE Beta (v Int64); INSERT INTO Beta VALUES (2), (4);")
        .unwrap();
    database
        .create_nullable_int64_table("Alpha", "v", vec![Some(1), None])
        .unwrap();
    database
        .create_int64_range_partitioned_table_with_limits(
            "Ranges",
            "v",
            vec![Int64RangePartition::new(10, 20, vec![10, 15])],
            Int64RangePartitionLimits::new(2, 8, 64),
        )
        .unwrap();
    database
        .create_int64_min_max_index("Alpha", "v", Default::default())
        .unwrap();

    database
        .enable_int64_write_ahead_log_registry(&["Ranges", "Beta", "Alpha"], &registry, limits())
        .unwrap();
    database
        .append_nullable_int64_values("alpha", &[None, Some(3)])
        .unwrap();
    database
        .replace_nullable_int64_values("Alpha", &[(0, None), (1, Some(9))])
        .unwrap();
    database
        .execute(
            "INSERT INTO Beta VALUES (6); \
             ALTER TABLE Beta UPDATE v = 5 WHERE v = 4; \
             INSERT INTO Ranges VALUES (18);",
        )
        .unwrap();
    assert_eq!(
        database
            .catalog()
            .table("Alpha")
            .unwrap()
            .int64_min_max_index_info()
            .unwrap()
            .indexed_rows,
        4
    );
    assert_eq!(
        database
            .catalog()
            .table("Ranges")
            .unwrap()
            .int64_range_partition_count(),
        None
    );

    let mut recovered =
        Database::recover_int64_write_ahead_log_registry(&registry, limits()).unwrap();
    let repeated = Database::recover_int64_write_ahead_log_registry(&registry, limits()).unwrap();
    assert_eq!(
        nullable(&recovered, "ALPHA"),
        [None, Some(9), None, Some(3)]
    );
    assert_eq!(non_nullable(&recovered, "beta"), [2, 5, 6]);
    assert_eq!(non_nullable(&recovered, "ranges"), [10, 15, 18]);
    assert_eq!(nullable(&repeated, "Alpha"), nullable(&recovered, "Alpha"));
    assert_eq!(recovered.catalog().table_count(), 3);
    assert!(!recovered.int64_write_ahead_log_enabled());

    let results = recovered.execute("SELECT v FROM Alpha ORDER BY v").unwrap();
    let [StatementResult::Query(result)] = results.as_slice() else {
        panic!("expected one query")
    };
    assert_eq!(
        result.rows,
        [
            vec![Value::Null(rusthouse::batch::value::DataType::Int64)],
            vec![Value::Null(rusthouse::batch::value::DataType::Int64)],
            vec![Value::Int64(3)],
            vec![Value::Int64(9)],
        ]
    );

    let manifest = fs::read(registry.join("manifest.rhi64")).unwrap();
    let text = String::from_utf8_lossy(&manifest);
    assert!(text.find("Alpha").unwrap() < text.find("Beta").unwrap());
    assert!(text.find("Beta").unwrap() < text.find("Ranges").unwrap());
}

#[test]
fn corrupt_missing_special_and_unlisted_members_fail_without_a_database() {
    let directory = TestDirectory::new();

    let corrupt = directory.join("corrupt");
    create_two_table_registry(&corrupt);
    let member = corrupt.join("table-00000001.wal");
    let mut bytes = fs::read(&member).unwrap();
    *bytes.last_mut().unwrap() ^= 0x20;
    fs::write(&member, bytes).unwrap();
    assert!(matches!(
        Database::recover_int64_write_ahead_log_registry(&corrupt, limits()),
        Err(DatabaseInt64WalRegistryRecoveryError::Registry(
            Int64WriteAheadLogRegistryError::Member {
                error: Int64WriteAheadLogError::Corruption(_),
                ..
            }
        ))
    ));

    let partial = directory.join("partial");
    create_two_table_registry(&partial);
    let partial_member = partial.join("table-00000001.wal");
    let partial_bytes = fs::read(&partial_member).unwrap();
    fs::write(&partial_member, &partial_bytes[..16]).unwrap();
    assert!(matches!(
        Database::recover_int64_write_ahead_log_registry(&partial, limits()),
        Err(DatabaseInt64WalRegistryRecoveryError::Registry(
            Int64WriteAheadLogRegistryError::Member {
                error: Int64WriteAheadLogError::Corruption(_),
                ..
            }
        ))
    ));

    let bad_manifest = directory.join("bad-manifest");
    create_two_table_registry(&bad_manifest);
    let manifest_path = bad_manifest.join("manifest.rhi64");
    let mut manifest_bytes = fs::read(&manifest_path).unwrap();
    *manifest_bytes.last_mut().unwrap() ^= 1;
    fs::write(&manifest_path, manifest_bytes).unwrap();
    assert!(matches!(
        Database::recover_int64_write_ahead_log_registry(&bad_manifest, limits()),
        Err(DatabaseInt64WalRegistryRecoveryError::Registry(
            Int64WriteAheadLogRegistryError::Corruption(
                Int64WriteAheadLogRegistryCorruption::ManifestChecksum { .. }
            )
        ))
    ));

    let missing = directory.join("missing");
    create_two_table_registry(&missing);
    fs::remove_file(missing.join("table-00000000.wal")).unwrap();
    assert!(matches!(
        Database::recover_int64_write_ahead_log_registry(&missing, limits()),
        Err(DatabaseInt64WalRegistryRecoveryError::Registry(
            Int64WriteAheadLogRegistryError::Corruption(
                Int64WriteAheadLogRegistryCorruption::MissingMember { .. }
            )
        ))
    ));

    let aliased = directory.join("aliased");
    create_two_table_registry(&aliased);
    fs::remove_file(aliased.join("table-00000001.wal")).unwrap();
    fs::hard_link(
        aliased.join("table-00000000.wal"),
        aliased.join("table-00000001.wal"),
    )
    .unwrap();
    assert!(matches!(
        Database::recover_int64_write_ahead_log_registry(&aliased, limits()),
        Err(DatabaseInt64WalRegistryRecoveryError::Registry(
            Int64WriteAheadLogRegistryError::Corruption(
                Int64WriteAheadLogRegistryCorruption::DuplicateMemberFile { .. }
            )
        ))
    ));

    let special = directory.join("special");
    create_two_table_registry(&special);
    let special_member = special.join("table-00000000.wal");
    fs::remove_file(&special_member).unwrap();
    let name = std::ffi::CString::new(special_member.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert!(matches!(
        Database::recover_int64_write_ahead_log_registry(&special, limits()),
        Err(DatabaseInt64WalRegistryRecoveryError::Registry(
            Int64WriteAheadLogRegistryError::Member {
                error: Int64WriteAheadLogError::NotRegularFile,
                ..
            }
        ))
    ));

    let unlisted = directory.join("unlisted");
    create_two_table_registry(&unlisted);
    fs::write(unlisted.join("extra.wal"), b"not listed").unwrap();
    assert!(matches!(
        Database::recover_int64_write_ahead_log_registry(&unlisted, limits()),
        Err(DatabaseInt64WalRegistryRecoveryError::Registry(
            Int64WriteAheadLogRegistryError::Corruption(
                Int64WriteAheadLogRegistryCorruption::UnexpectedDirectoryEntry { .. }
            )
        ))
    ));
}

#[test]
fn duplicate_inputs_and_aggregate_limits_are_typed_and_prepublication() {
    let directory = TestDirectory::new();
    let duplicate_path = directory.join("duplicate");
    let mut database = Database::new();
    database.execute("CREATE TABLE Events (v Int64)").unwrap();
    assert!(matches!(
        database.enable_int64_write_ahead_log_registry(
            &["Events", "events"],
            &duplicate_path,
            limits(),
        ),
        Err(DatabaseInt64WalRegistryEnableError::Registry(
            Int64WriteAheadLogRegistryError::Corruption(
                Int64WriteAheadLogRegistryCorruption::DuplicateTable { .. }
            )
        ))
    ));
    assert!(!duplicate_path.exists());
    assert!(!database.int64_write_ahead_log_enabled());

    let registry = directory.join("bounded");
    create_two_table_registry(&registry);
    let too_few_tables = Int64WriteAheadLogRegistryLimits {
        max_tables: 1,
        ..limits()
    };
    assert!(matches!(
        Database::recover_int64_write_ahead_log_registry(&registry, too_few_tables),
        Err(DatabaseInt64WalRegistryRecoveryError::Registry(
            Int64WriteAheadLogRegistryError::Limit(Int64WriteAheadLogRegistryLimitError::Tables {
                tables: 2,
                max_tables: 1,
            })
        ))
    ));

    let too_few_records = Int64WriteAheadLogRegistryLimits {
        max_total_records: 1,
        ..limits()
    };
    assert!(matches!(
        Database::recover_int64_write_ahead_log_registry(&registry, too_few_records),
        Err(DatabaseInt64WalRegistryRecoveryError::Registry(
            Int64WriteAheadLogRegistryError::Limit(
                Int64WriteAheadLogRegistryLimitError::TotalRecords { .. }
            )
        ))
    ));

    let too_few_bytes = Int64WriteAheadLogRegistryLimits {
        max_total_wal_bytes: 1,
        ..limits()
    };
    assert!(matches!(
        Database::recover_int64_write_ahead_log_registry(&registry, too_few_bytes),
        Err(DatabaseInt64WalRegistryRecoveryError::Registry(
            Int64WriteAheadLogRegistryError::Limit(
                Int64WriteAheadLogRegistryLimitError::TotalWalBytes { .. }
            )
        ))
    ));
}

fn checksum_update(mut checksum: u32, bytes: &[u8]) -> u32 {
    for byte in bytes {
        checksum ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0_u32.wrapping_sub(checksum & 1);
            checksum = (checksum >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    checksum
}

fn manifest(descriptors: &[(&str, &str)]) -> Vec<u8> {
    let mut payload = Vec::new();
    for (table, member) in descriptors {
        payload.extend_from_slice(&(table.len() as u32).to_le_bytes());
        payload.extend_from_slice(table.as_bytes());
        payload.extend_from_slice(&(member.len() as u32).to_le_bytes());
        payload.extend_from_slice(member.as_bytes());
    }
    let mut checksum = u32::MAX;
    checksum = checksum_update(checksum, &1_u16.to_le_bytes());
    checksum = checksum_update(checksum, &0_u16.to_le_bytes());
    checksum = checksum_update(checksum, &(descriptors.len() as u32).to_le_bytes());
    checksum = checksum_update(checksum, &(payload.len() as u64).to_le_bytes());
    checksum = checksum_update(checksum, &payload);
    let mut output = b"RHI64REG".to_vec();
    output.extend_from_slice(&1_u16.to_le_bytes());
    output.extend_from_slice(&0_u16.to_le_bytes());
    output.extend_from_slice(&(descriptors.len() as u32).to_le_bytes());
    output.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    output.extend_from_slice(&(!checksum).to_le_bytes());
    output.extend_from_slice(&payload);
    output
}

#[test]
fn manifest_rejects_duplicate_colliding_and_traversing_descriptors() {
    let directory = TestDirectory::new();
    let registry = directory.join("registry");
    create_two_table_registry(&registry);
    let manifest_path = registry.join("manifest.rhi64");

    for (case, descriptors, expected) in [
        (
            "duplicate case-insensitive",
            vec![
                ("Alpha", "table-00000000.wal"),
                ("alpha", "table-00000001.wal"),
            ],
            "duplicate case-insensitive",
        ),
        (
            "member collision",
            vec![
                ("Alpha", "table-00000000.wal"),
                ("Beta", "TABLE-00000000.WAL"),
            ],
            "duplicate member",
        ),
        (
            "traversal",
            vec![("Alpha", "../escape.wal"), ("Beta", "table-00000001.wal")],
            "safe single path component",
        ),
    ] {
        fs::write(&manifest_path, manifest(&descriptors)).unwrap();
        let error =
            Database::recover_int64_write_ahead_log_registry(&registry, limits()).expect_err(case);
        assert!(error.to_string().contains(expected), "{case}: {error}");
    }
}

#[test]
fn registry_limits_reject_mutation_before_memory_publication() {
    let directory = TestDirectory::new();
    let registry = directory.join("registry");
    let mut database = Database::new();
    database.execute("CREATE TABLE Events (v Int64)").unwrap();
    let one_record = Int64WriteAheadLogRegistryLimits {
        max_total_records: 1,
        ..limits()
    };
    database
        .enable_int64_write_ahead_log_registry(&["Events"], &registry, one_record)
        .unwrap();
    let error = database
        .execute("INSERT INTO Events VALUES (1)")
        .unwrap_err();
    assert!(error.to_string().contains("aggregate limit of 1"));
    assert!(non_nullable(&database, "Events").is_empty());
}
