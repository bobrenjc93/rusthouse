use std::env;
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::time::{Duration, Instant};

#[test]
fn ordinary_binary_refuses_unattested_benchmark_identity() {
    let output = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .arg("--benchmark-attestation")
        .output()
        .expect("run CLI");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .expect("UTF-8 stderr")
            .contains("attested build token is unavailable")
    );
}

#[test]
fn cleanup_guard_removes_staging_directory_after_parent_disconnect() {
    let directory = env::temp_dir().join(format!(
        "rusthouse-benchmark-pinned-{}-cleanup-test",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir(&directory).expect("staging directory");
    fs::write(directory.join("clickhouse-pinned"), b"abandoned artifact").expect("staged artifact");

    let mut guardian = Command::new(env!("CARGO_BIN_EXE_clickhouse-parity-bench"))
        .arg("--internal-staging-cleanup-guard")
        .arg(&directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cleanup guardian");
    drop(guardian.stdin.take());
    let output = guardian
        .wait_with_output()
        .expect("wait for cleanup guardian");
    assert!(
        output.status.success(),
        "cleanup guardian failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!directory.exists());
}

#[cfg(unix)]
#[test]
fn harness_path_replacement_does_not_change_running_staged_bytes() {
    use std::os::unix::fs::PermissionsExt as _;

    let directory = env::temp_dir().join(format!(
        "rusthouse-harness-replacement-test-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir(&directory).expect("replacement test directory");
    let harness = directory.join("clickhouse-parity-bench");
    fs::copy(env!("CARGO_BIN_EXE_clickhouse-parity-bench"), &harness)
        .expect("copy benchmark harness");
    fs::set_permissions(&harness, fs::Permissions::from_mode(0o700)).expect("harness permissions");
    let child = Command::new(&harness)
        .arg("--help")
        .env("RUSTHOUSE_TEST_STAGED_HARNESS_DELAY_MS", "500")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn benchmark launcher");
    let staging_prefix = format!("rusthouse-benchmark-harness-{}-", child.id());
    let deadline = Instant::now() + Duration::from_secs(10);
    let staging_directory = loop {
        if let Some(path) = fs::read_dir(env::temp_dir())
            .expect("temporary directory")
            .flatten()
            .find(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(&staging_prefix))
            })
            .map(|entry| entry.path())
        {
            break path;
        }
        assert!(Instant::now() < deadline, "harness was not staged");
        std::thread::sleep(Duration::from_millis(10));
    };

    let replacement = directory.join("replacement");
    fs::write(&replacement, b"replacement bytes").expect("replacement file");
    fs::rename(&replacement, &harness).expect("replace launched harness path");
    let output = child.wait_with_output().expect("wait for staged harness");
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("USAGE:"));
    assert_eq!(
        fs::read(&harness).expect("replacement bytes"),
        b"replacement bytes"
    );
    assert!(!staging_directory.exists());
    fs::remove_dir_all(directory).expect("cleanup replacement test");
}

#[test]
fn execute_argument_emits_clean_json_and_command_statuses() {
    let output = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args([
            "--format=json",
            "--execute",
            "CREATE TABLE items (name String, n Int64);
             INSERT INTO items VALUES ('b', 2), ('a', 1);
             SELECT name, n FROM items ORDER BY n;",
        ])
        .output()
        .expect("run CLI");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8 stdout"),
        "{\"results\":[{\"columns\":[{\"name\":\"name\",\"type\":\"String\"},{\"name\":\"n\",\"type\":\"Int64\"}],\"rows\":[[\"a\",1],[\"b\",2]]}]}\n"
    );
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(stderr.contains("CREATE TABLE"));
    assert!(stderr.contains("INSERT 2"));
}

#[test]
fn multiple_selects_emit_one_json_document() {
    let output = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args([
            "--format=json",
            "--execute",
            "CREATE TABLE numbers (n Int64);
             INSERT INTO numbers VALUES (1), (2);
             SELECT n FROM numbers WHERE n = 1;
             SELECT n FROM numbers WHERE n = 2;",
        ])
        .output()
        .expect("run CLI");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8 stdout"),
        "{\"results\":[{\"columns\":[{\"name\":\"n\",\"type\":\"Int64\"}],\"rows\":[[1]]},{\"columns\":[{\"name\":\"n\",\"type\":\"Int64\"}],\"rows\":[[2]]}]}\n"
    );
}

#[test]
fn positional_json_preserves_duplicate_alias_values() {
    let output = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args([
            "--format=json",
            "--execute",
            "CREATE TABLE items (id Int64, label String);
             INSERT INTO items VALUES (1, 'one');
             SELECT id, label AS id FROM items;",
        ])
        .output()
        .expect("run CLI");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8 stdout"),
        "{\"results\":[{\"columns\":[{\"name\":\"id\",\"type\":\"Int64\"},{\"name\":\"id\",\"type\":\"String\"}],\"rows\":[[1,\"one\"]]}]}\n"
    );
}

#[test]
fn stdin_and_csv_output_work_together() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args(["--format", "csv"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn CLI");

    child
        .stdin
        .take()
        .expect("stdin pipe")
        .write_all(
            b"CREATE TABLE notes (label String, active Bool);
              INSERT INTO notes VALUES ('hello, world', true);
              SELECT * FROM notes;",
        )
        .expect("write SQL");

    let output = child.wait_with_output().expect("wait for CLI");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8 stdout"),
        "label,active\n\"hello, world\",true\n"
    );
}

#[test]
fn sql_errors_are_reported_with_nonzero_status() {
    let output = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args([
            "--execute",
            "CREATE TABLE t (id Int64); INSERT INTO t VALUES ('wrong');",
        ])
        .output()
        .expect("run CLI");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(stderr.contains("type mismatch for column 't.id'"));
    assert!(stderr.contains("expected Int64, found String"));
}

#[test]
fn excessive_predicates_return_cli_errors_without_aborting() {
    let cases = [
        (
            format!(
                "SELECT id FROM things WHERE {}id = 1{}",
                "(".repeat(50_000),
                ")".repeat(50_000)
            ),
            "predicate nesting exceeds limit of 64",
        ),
        (
            format!(
                "SELECT id FROM things WHERE {}",
                vec!["id = 1"; 50_000].join(" OR ")
            ),
            "predicate is too complex; maximum 256 expression nodes",
        ),
    ];

    for (sql, expected_error) in cases {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn CLI");
        child
            .stdin
            .take()
            .expect("stdin pipe")
            .write_all(sql.as_bytes())
            .expect("write large SQL query");

        let output = child.wait_with_output().expect("wait for CLI");
        assert!(!output.status.success());
        let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
        assert!(
            stderr.contains(expected_error),
            "unexpected stderr: {stderr}"
        );
        assert!(!stderr.contains("stack overflow"));
    }
}
