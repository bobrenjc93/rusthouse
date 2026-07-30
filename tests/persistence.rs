use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use rusthouse::{Database, Error, QueryResult, StatementResult, Value};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TempDirectory {
    path: PathBuf,
}

impl TempDirectory {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "rusthouse-{label}-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("create temporary directory");
        Self { path }
    }

    fn database(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn query(database: &mut Database, sql: &str) -> QueryResult {
    match database
        .execute(sql)
        .expect("query succeeds")
        .into_iter()
        .last()
        .expect("query result")
    {
        StatementResult::Query(result) => result,
        StatementResult::Command { .. } => panic!("expected query result"),
    }
}

fn create_snapshot(path: &Path) {
    let mut database = Database::open(path).expect("open new database");
    database
        .execute(
            "CREATE TABLE samples (id Int64, score Float64, active Bool, label String);
             INSERT INTO samples VALUES (7, 2.5, true, 'saved');",
        )
        .expect("create persisted data");
}

#[test]
fn cli_database_option_persists_all_types_across_processes() {
    let directory = TempDirectory::new("cli-restart");
    let path = directory.database("catalog.rsh");

    let setup = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args([
            "--database",
            path.to_str().expect("UTF-8 path"),
            "--execute",
            "CREATE TABLE readings (id Int64, score Float64, active Bool, label String);
             INSERT INTO readings VALUES (2, 1.25, false, 'second'), (1, -0.5, true, 'first');",
        ])
        .output()
        .expect("run setup process");
    assert!(
        setup.status.success(),
        "setup stderr: {}",
        String::from_utf8_lossy(&setup.stderr)
    );

    let query = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args([
            "--database",
            path.to_str().expect("UTF-8 path"),
            "--format=json",
            "--execute",
            "SELECT id, score, active, label FROM readings ORDER BY id",
        ])
        .output()
        .expect("run query process");
    assert!(
        query.status.success(),
        "query stderr: {}",
        String::from_utf8_lossy(&query.stderr)
    );
    assert_eq!(
        String::from_utf8(query.stdout).expect("UTF-8 stdout"),
        "{\"results\":[{\"columns\":[{\"name\":\"id\",\"type\":\"Int64\"},{\"name\":\"score\",\"type\":\"Float64\"},{\"name\":\"active\",\"type\":\"Bool\"},{\"name\":\"label\",\"type\":\"String\"}],\"rows\":[[1,-0.5,true,\"first\"],[2,1.25,false,\"second\"]]}]}\n"
    );
}

#[test]
fn execution_failure_persists_the_successful_batch_prefix() {
    let directory = TempDirectory::new("partial-batch");
    let path = directory.database("catalog.rsh");
    let mut database = Database::open(&path).expect("open database");

    let error = database
        .execute(
            "CREATE TABLE events (id Int64);
             INSERT INTO events VALUES (1), (2);
             INSERT INTO events VALUES (false);",
        )
        .expect_err("last statement fails");
    assert!(matches!(error, Error::TypeMismatch { .. }));
    drop(database);

    let mut restarted = Database::open(path).expect("reopen persisted prefix");
    assert_eq!(
        query(&mut restarted, "SELECT id FROM events ORDER BY id").rows,
        vec![vec![Value::Int64(1)], vec![Value::Int64(2)]]
    );
}

#[test]
fn parse_failure_does_not_create_or_change_a_snapshot() {
    let directory = TempDirectory::new("parse-failure");
    let path = directory.database("catalog.rsh");
    let mut database = Database::open(&path).expect("open new database");

    let error = database
        .execute("CREATE TABLE discarded (id Int64); SELECT id FORM discarded")
        .expect_err("batch has a syntax error");
    assert!(matches!(error, Error::Sql { .. }));
    assert!(!path.exists());
}

#[test]
fn corrupt_truncated_and_unsupported_snapshots_are_rejected() {
    let directory = TempDirectory::new("invalid-snapshots");
    let valid_path = directory.database("valid.rsh");
    create_snapshot(&valid_path);
    let valid = fs::read(&valid_path).expect("read valid snapshot");

    let corrupt_path = directory.database("corrupt.rsh");
    let mut corrupt = valid.clone();
    let last = corrupt.last_mut().expect("nonempty snapshot");
    *last ^= 0x80;
    fs::write(&corrupt_path, corrupt).expect("write corrupt snapshot");
    assert!(matches!(
        Database::open(&corrupt_path),
        Err(Error::InvalidSnapshot { message, .. }) if message.contains("checksum")
    ));

    let truncated_path = directory.database("truncated.rsh");
    fs::write(&truncated_path, &valid[..valid.len() / 2]).expect("write truncated snapshot");
    assert!(matches!(
        Database::open(&truncated_path),
        Err(Error::InvalidSnapshot { message, .. }) if message.contains("truncated")
    ));

    let unsupported_path = directory.database("unsupported.rsh");
    let mut unsupported = valid;
    unsupported[8..12].copy_from_slice(&2_u32.to_le_bytes());
    fs::write(&unsupported_path, unsupported).expect("write unsupported snapshot");
    assert!(matches!(
        Database::open(&unsupported_path),
        Err(Error::UnsupportedSnapshotVersion {
            version: 2,
            supported: 1,
            ..
        })
    ));
}

#[test]
fn failed_checkpoint_rolls_back_memory_and_leaves_previous_snapshot_intact() {
    let directory = TempDirectory::new("failed-write");
    let live_directory = directory.path.join("live");
    fs::create_dir(&live_directory).expect("create live directory");
    let live_path = live_directory.join("catalog.rsh");
    create_snapshot(&live_path);
    let original = fs::read(&live_path).expect("read original snapshot");

    let mut database = Database::open(&live_path).expect("open existing database");
    let saved_directory = directory.path.join("saved");
    fs::rename(&live_directory, &saved_directory).expect("make checkpoint parent unavailable");

    let error = database
        .execute("INSERT INTO samples VALUES (8, 3.5, false, 'not saved')")
        .expect_err("checkpoint cannot create its temporary file");
    assert!(matches!(error, Error::Persistence { .. }));
    assert_eq!(
        query(&mut database, "SELECT COUNT(*) AS count FROM samples").rows,
        vec![vec![Value::Int64(1)]]
    );

    let saved_path = saved_directory.join("catalog.rsh");
    assert_eq!(
        fs::read(&saved_path).expect("read saved snapshot"),
        original
    );
    let mut restarted = Database::open(saved_path).expect("old snapshot remains loadable");
    assert_eq!(
        query(&mut restarted, "SELECT COUNT(*) AS count FROM samples").rows,
        vec![vec![Value::Int64(1)]]
    );
}

#[test]
fn logically_identical_catalogs_have_deterministic_snapshot_bytes() {
    let directory = TempDirectory::new("deterministic");
    let first_path = directory.database("first.rsh");
    let second_path = directory.database("second.rsh");

    let mut first = Database::open(&first_path).expect("open first database");
    first
        .execute(
            "CREATE TABLE Alpha (id Int64, label String);
             INSERT INTO Alpha VALUES (1, 'a');
             CREATE TABLE beta (enabled Bool);
             INSERT INTO beta VALUES (true);",
        )
        .expect("populate first database");

    let mut second = Database::open(&second_path).expect("open second database");
    second
        .execute(
            "CREATE TABLE beta (enabled Bool);
             INSERT INTO beta VALUES (true);
             CREATE TABLE Alpha (id Int64, label String);
             INSERT INTO Alpha VALUES (1, 'a');",
        )
        .expect("populate second database");

    let first_bytes = fs::read(&first_path).expect("read first snapshot");
    assert_eq!(
        first_bytes,
        fs::read(second_path).expect("read second snapshot")
    );

    let round_tripped = Database::open(&first_path).expect("load snapshot for round trip");
    round_tripped.checkpoint().expect("rewrite snapshot");
    assert_eq!(
        fs::read(first_path).expect("read round-tripped snapshot"),
        first_bytes
    );
}

#[cfg(unix)]
#[test]
fn checkpoints_create_private_files_and_preserve_existing_unix_metadata() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let directory = TempDirectory::new("permissions");
    let path = directory.database("catalog.rsh");
    create_snapshot(&path);
    let private_metadata = fs::metadata(&path).expect("read new snapshot metadata");
    assert_eq!(private_metadata.mode() & 0o077, 0);

    fs::set_permissions(&path, fs::Permissions::from_mode(0o640))
        .expect("set database permissions");
    let before = fs::metadata(&path).expect("read metadata before checkpoint");
    let mut database = Database::open(&path).expect("open existing database");
    database
        .execute("INSERT INTO samples VALUES (8, 3.5, false, 'saved securely')")
        .expect("checkpoint database");
    let after = fs::metadata(path).expect("read metadata after checkpoint");

    assert_eq!(after.mode() & 0o7777, 0o640);
    assert_eq!(after.uid(), before.uid());
    assert_eq!(after.gid(), before.gid());
}
