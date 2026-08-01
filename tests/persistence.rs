use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusthouse::{Database, Error, SnapshotError, SnapshotStore, StatementResult, Value};

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
    let _ = fs::remove_file(lock_path(path));
}

fn lock_path(path: &Path) -> PathBuf {
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".rusthouse-lock");
    PathBuf::from(lock)
}

#[cfg(target_os = "macos")]
fn install_inheritable_test_acl(path: &Path) {
    use exacl::{AclEntry, Flag, Perm};

    let acl = [AclEntry::allow_user(
        "nobody",
        Perm::READ | Perm::EXECUTE,
        Flag::FILE_INHERIT | Flag::DIRECTORY_INHERIT,
    )];
    exacl::setfacl(&[path], &acl, None).unwrap();
}

#[cfg(target_os = "linux")]
fn install_inheritable_test_acl(path: &Path) {
    use exacl::{AclEntry, Flag, Perm};

    let mut acl = exacl::getfacl(path, None).unwrap();
    acl.extend([
        AclEntry::allow_user("", Perm::READ | Perm::WRITE | Perm::EXECUTE, Flag::DEFAULT),
        AclEntry::allow_user("nobody", Perm::READ | Perm::EXECUTE, Flag::DEFAULT),
        AclEntry::allow_group("", Perm::empty(), Flag::DEFAULT),
        AclEntry::allow_mask(Perm::READ | Perm::EXECUTE, Flag::DEFAULT),
        AclEntry::allow_other(Perm::empty(), Flag::DEFAULT),
    ]);
    exacl::setfacl(&[path], &acl, None).unwrap();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn acl_allows_nobody(path: &Path) -> bool {
    exacl::getfacl(path, None)
        .unwrap()
        .iter()
        .any(|entry| entry.name.ends_with("nobody") && entry.perms.contains(exacl::Perm::READ))
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
fn database_and_snapshot_store_share_lock_and_reserved_namespaces() {
    let path = temporary_path("cross-api-lock");
    let lock = lock_path(&path);
    let database = Database::open(&path).unwrap();
    assert!(matches!(
        SnapshotStore::open(&path),
        Err(SnapshotError::Locked(_))
    ));
    assert!(matches!(
        SnapshotStore::open(&lock),
        Err(SnapshotError::ReservedSnapshotName(_))
    ));
    assert!(matches!(
        Database::open(&lock),
        Err(Error::ReservedDatabasePath(_))
    ));
    drop(database);

    let snapshot = SnapshotStore::open(&path).unwrap();
    assert!(matches!(
        Database::open(&path),
        Err(Error::DatabaseAlreadyOpen(_))
    ));
    let catalog_temp = path.parent().unwrap().join(format!(
        ".{}.tmp",
        path.file_name().unwrap().to_string_lossy()
    ));
    assert!(matches!(
        Database::open(&catalog_temp),
        Err(Error::ReservedDatabasePath(_))
    ));
    drop(snapshot);
    remove_database(&path);
}

#[cfg(unix)]
#[test]
fn parent_replacement_keeps_database_writers_and_snapshots_isolated() {
    let root = temporary_path("parent-replacement").with_extension("dir");
    let active_parent = root.join("active");
    let moved_parent = root.join("moved");
    fs::create_dir_all(&active_parent).unwrap();
    let active_path = active_parent.join("database.db");
    let moved_path = moved_parent.join("database.db");
    let first = Database::open(&active_path).unwrap();

    fs::rename(&active_parent, &moved_parent).unwrap();
    assert!(matches!(
        Database::open(&moved_path),
        Err(Error::DatabaseAlreadyOpen(_))
    ));
    fs::create_dir(&active_parent).unwrap();
    let second = Database::open(&active_path).unwrap();

    first.execute("CREATE TABLE original (id Int64)").unwrap();
    second
        .execute("CREATE TABLE replacement (id Int64)")
        .unwrap();
    drop(first);
    drop(second);

    let original = Database::open(&moved_path).unwrap();
    assert!(matches!(
        original.execute("SELECT * FROM original"),
        Ok(StatementResult::Query(_))
    ));
    assert!(matches!(
        original.execute("SELECT * FROM replacement"),
        Err(Error::TableNotFound(_))
    ));
    drop(original);

    let replacement = Database::open(&active_path).unwrap();
    assert!(matches!(
        replacement.execute("SELECT * FROM replacement"),
        Ok(StatementResult::Query(_))
    ));
    assert!(matches!(
        replacement.execute("SELECT * FROM original"),
        Err(Error::TableNotFound(_))
    ));
    drop(replacement);

    fs::remove_dir_all(root).unwrap();
}

#[cfg(windows)]
#[test]
fn windows_parent_directory_replacement_is_blocked_until_drop() {
    let root = temporary_path("windows-parent-guard").with_extension("dir");
    let active_parent = root.join("active");
    let moved_parent = root.join("moved");
    fs::create_dir_all(&active_parent).unwrap();
    let active_path = active_parent.join("database.db");
    let moved_path = moved_parent.join("database.db");
    let database = Database::open(&active_path).unwrap();

    assert!(fs::rename(&active_parent, &moved_parent).is_err());
    assert!(active_parent.exists());
    assert!(!moved_parent.exists());
    database.execute("CREATE TABLE guarded (id Int64)").unwrap();
    assert!(matches!(
        database.execute("SELECT * FROM guarded"),
        Ok(StatementResult::Query(_))
    ));

    drop(database);
    fs::rename(&active_parent, &moved_parent).unwrap();
    let reopened = Database::open(&moved_path).unwrap();
    assert!(matches!(
        reopened.execute("SELECT * FROM guarded"),
        Ok(StatementResult::Query(_))
    ));
    drop(reopened);
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn lock_symlink_cannot_redirect_ownership_to_the_database() {
    use std::os::unix::fs::symlink;

    let path = temporary_path("lock-symlink");
    let database = Database::open(&path).unwrap();
    database.execute("CREATE TABLE intact (id Int64)").unwrap();
    drop(database);

    let lock = lock_path(&path);
    fs::remove_file(&lock).unwrap();
    symlink(&path, &lock).unwrap();
    assert!(matches!(
        Database::open(&path),
        Err(Error::UnsafeLockPath(_))
    ));
    fs::remove_file(&lock).unwrap();

    let reopened = Database::open(&path).unwrap();
    assert!(matches!(
        reopened.execute("SELECT * FROM intact"),
        Ok(StatementResult::Query(_))
    ));
    drop(reopened);
    remove_database(&path);
}

#[test]
fn database_open_requires_an_existing_parent_directory() {
    let missing_parent = temporary_path("missing-parent");
    let path = missing_parent.join("database.db");
    assert!(matches!(Database::open(&path), Err(Error::Io { .. })));
    assert!(!missing_parent.exists());
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
fn temporary_snapshot_namespace_cannot_be_another_database() {
    let path = temporary_path("temp-namespace");
    let owner = Database::open(&path).unwrap();
    let generated_shape = path.parent().unwrap().join(format!(
        ".rusthouse-tmp.{}.999999.backup",
        std::process::id()
    ));
    assert!(matches!(
        Database::open(&generated_shape),
        Err(Error::ReservedDatabasePath(_))
    ));

    owner.execute("CREATE TABLE original (id Int64)").unwrap();
    drop(owner);
    let reopened = Database::open(&path).unwrap();
    assert!(matches!(
        reopened.execute("SELECT * FROM original"),
        Ok(StatementResult::Query(_))
    ));
    drop(reopened);
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn inherited_parent_acl_does_not_reach_first_database_snapshot() {
    let root = temporary_path("inherited-security").with_extension("dir");
    fs::create_dir(&root).unwrap();
    install_inheritable_test_acl(&root);
    let probe = root.join("probe");
    fs::write(&probe, b"probe").unwrap();
    assert!(acl_allows_nobody(&probe));
    fs::remove_file(probe).unwrap();

    let path = root.join("database.db");
    let database = Database::open(&path).unwrap();
    database
        .execute("CREATE TABLE private_data (id Int64)")
        .unwrap();
    assert!(!acl_allows_nobody(&path));

    drop(database);
    fs::remove_dir_all(root).unwrap();
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
    assert!(matches!(
        database.execute("INSERT INTO windows_data VALUES (1)"),
        Err(Error::CommitDurabilityUncertain { generation: 2, .. })
    ));
    assert_eq!(database.current_generation().unwrap(), 2);
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
