use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rusthouse::{Database, Error, StatementResult, Value};

static NEXT_PATH: AtomicU64 = AtomicU64::new(0);

fn temporary_path(name: &str) -> PathBuf {
    let sequence = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "rusthouse-{name}-{}-{sequence}.db",
        std::process::id()
    ))
}

fn remove_database(path: &PathBuf) {
    let _ = fs::remove_file(path);
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".rusthouse-lock");
    let _ = fs::remove_file(PathBuf::from(lock));
}

#[test]
fn committed_generation_reopens_with_schema_and_rows() {
    let path = temporary_path("reopen");
    {
        let database = Database::open(&path).unwrap();
        let mut session = database.session();
        session.begin().unwrap();
        session
            .execute("CREATE TABLE durable (id Int64, ok Bool, label String NULL)")
            .unwrap();
        session
            .execute("INSERT INTO durable VALUES (1, true, 'one'), (2, false, NULL)")
            .unwrap();
        assert_eq!(session.commit().unwrap(), 1);
    }

    let reopened = Database::open(&path).unwrap();
    assert_eq!(reopened.current_generation().unwrap(), 1);
    let StatementResult::Query(result) = reopened
        .execute("SELECT label, id FROM durable WHERE ok = false")
        .unwrap()
    else {
        panic!("expected query result");
    };
    assert_eq!(result.rows, vec![vec![Value::Null, Value::Int64(2)]]);
    drop(reopened);
    remove_database(&path);
}

#[test]
fn corrupt_snapshot_is_rejected_without_recovery_guessing() {
    let path = temporary_path("corrupt");
    fs::write(&path, b"not a rusthouse snapshot").unwrap();
    assert!(matches!(
        Database::open(&path),
        Err(Error::CorruptSnapshot(_))
    ));
    remove_database(&path);
}

#[test]
fn a_persisted_path_has_one_exclusive_owner() {
    let path = temporary_path("exclusive");
    let owner = Database::open(&path).unwrap();
    owner.execute("CREATE TABLE first (id Int64)").unwrap();

    assert!(matches!(
        Database::open(&path),
        Err(Error::DatabaseAlreadyOpen(_))
    ));
    drop(owner);

    let next_owner = Database::open(&path).unwrap();
    assert!(matches!(
        next_owner.execute("SELECT * FROM first"),
        Ok(StatementResult::Query(_))
    ));
    drop(next_owner);
    remove_database(&path);
}

#[test]
fn database_names_cannot_replace_another_databases_lock() {
    let path = temporary_path("lock-namespace");
    let lock_named_database = path.with_extension("db.lock");
    let owner = Database::open(&path).unwrap();
    let other = Database::open(&lock_named_database).unwrap();
    other
        .execute("CREATE TABLE independent (id Int64)")
        .unwrap();

    assert!(matches!(
        Database::open(&path),
        Err(Error::DatabaseAlreadyOpen(_))
    ));
    let mut reserved = path.as_os_str().to_os_string();
    reserved.push(".rusthouse-lock");
    assert!(matches!(
        Database::open(PathBuf::from(reserved)),
        Err(Error::ReservedDatabasePath(_))
    ));

    drop(other);
    drop(owner);
    remove_database(&lock_named_database);
    remove_database(&path);
}

#[test]
fn writer_rejects_catalog_shape_that_reopen_would_reject() {
    let path = temporary_path("wide");
    let database = Database::open(&path).unwrap();
    let mut session = database.session();
    session.begin().unwrap();
    let columns = (0..4_097)
        .map(|index| format!("c{index} Int64"))
        .collect::<Vec<_>>();
    session
        .execute(&format!("CREATE TABLE too_wide ({})", columns.join(", ")))
        .unwrap();

    assert!(matches!(
        session.commit(),
        Err(Error::SnapshotLimitExceeded {
            resource: "columns per table",
            limit: 4_096,
            attempted: 4_097,
        })
    ));
    assert!(session.in_transaction());
    session.rollback().unwrap();

    session.begin().unwrap();
    session
        .execute(&format!(
            "CREATE TABLE widest_supported ({})",
            columns[..4_096].join(", ")
        ))
        .unwrap();
    assert_eq!(session.commit().unwrap(), 1);
    drop(session);
    drop(database);

    let reopened = Database::open(&path).unwrap();
    assert_eq!(reopened.current_generation().unwrap(), 1);
    assert!(matches!(
        reopened.execute("SELECT * FROM too_wide"),
        Err(Error::TableNotFound(_))
    ));
    let StatementResult::Query(result) =
        reopened.execute("SELECT * FROM widest_supported").unwrap()
    else {
        panic!("expected query result");
    };
    assert_eq!(result.columns.len(), 4_096);
    drop(reopened);
    remove_database(&path);
}

#[cfg(unix)]
#[test]
fn replacement_is_private_and_preserves_existing_mode() {
    use std::os::unix::fs::PermissionsExt;

    let path = temporary_path("permissions");
    let database = Database::open(&path).unwrap();
    database
        .execute("CREATE TABLE private_data (id Int64)")
        .unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    database
        .execute("INSERT INTO private_data VALUES (1)")
        .unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    #[cfg(any(target_os = "freebsd", target_os = "linux", target_os = "macos"))]
    if let Ok(user) = std::env::var("USER") {
        #[cfg(any(target_os = "freebsd", target_os = "linux"))]
        use exacl::AclEntryKind;
        use exacl::{AclEntry, Perm};

        let mut acl = exacl::getfacl(&path, None).unwrap();
        acl.push(AclEntry::allow_user(&user, Perm::empty(), None));
        #[cfg(any(target_os = "freebsd", target_os = "linux"))]
        if !acl.iter().any(|entry| entry.kind == AclEntryKind::Mask) {
            acl.push(AclEntry::allow_mask(Perm::empty(), None));
        }
        exacl::setfacl(&[&path], &acl, None).unwrap();
        let mut expected_acl = exacl::getfacl(&path, None).unwrap();
        expected_acl.sort();
        let expected_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;

        database
            .execute("INSERT INTO private_data VALUES (2)")
            .unwrap();
        let mut actual_acl = exacl::getfacl(&path, None).unwrap();
        actual_acl.sort();
        assert_eq!(actual_acl, expected_acl);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            expected_mode
        );
    }
    drop(database);
    remove_database(&path);
}

#[cfg(unix)]
#[test]
fn replacement_preserves_owner_and_group() {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;

    let path = temporary_path("ownership");
    let database = Database::open(&path).unwrap();
    database
        .execute("CREATE TABLE owned_data (id Int64)")
        .unwrap();

    let original = fs::metadata(&path).unwrap();
    if unsafe { libc::geteuid() } == 0 {
        let alternate_gid = if original.gid() == 250 { 251 } else { 250 };
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        // Root-only coverage reproduces a destination group unlike the process group.
        assert_eq!(
            unsafe { libc::fchown(file.as_raw_fd(), original.uid(), alternate_gid) },
            0
        );
    }
    let expected = fs::metadata(&path).unwrap();

    database
        .execute("INSERT INTO owned_data VALUES (1)")
        .unwrap();
    let actual = fs::metadata(&path).unwrap();
    assert_eq!(actual.uid(), expected.uid());
    assert_eq!(actual.gid(), expected.gid());
    drop(database);
    remove_database(&path);
}

#[cfg(windows)]
#[test]
fn windows_native_replacement_publishes_repeated_commits() {
    let path = temporary_path("windows-replace");
    let database = Database::open(&path).unwrap();
    database
        .execute("CREATE TABLE windows_data (id Int64)")
        .unwrap();
    database
        .execute("INSERT INTO windows_data VALUES (1)")
        .unwrap();
    drop(database);

    let reopened = Database::open(&path).unwrap();
    let StatementResult::Query(result) = reopened.execute("SELECT * FROM windows_data").unwrap()
    else {
        panic!("expected query result");
    };
    assert_eq!(result.rows, vec![vec![Value::Int64(1)]]);
    drop(reopened);
    remove_database(&path);
}
