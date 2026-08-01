use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_PATH: AtomicU64 = AtomicU64::new(0);

fn database_path() -> PathBuf {
    let sequence = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "rusthouse-cli-{}-{sequence}.db",
        std::process::id()
    ))
}

fn remove_database(path: &PathBuf) {
    let _ = fs::remove_file(path);
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".rusthouse-lock");
    let _ = fs::remove_file(PathBuf::from(lock));
}

fn run_with_stdin(input: &str, arguments: &[&str]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn repeated_execute_arguments_share_a_transaction_and_persist() {
    let path = database_path();
    let output = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .arg("--database")
        .arg(&path)
        .args([
            "-e",
            "BEGIN",
            "-e",
            "CREATE TABLE cli_events (id Int64, label String)",
            "-e",
            "INSERT INTO cli_events VALUES (3, 'from cli')",
            "-e",
            "COMMIT",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("COMMIT (generation 1)")
    );

    let output = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .arg("--database")
        .arg(&path)
        .args(["-e", "SELECT * FROM cli_events"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("id\tlabel\n3\tfrom cli\n1 row(s)"));
    remove_database(&path);
}

#[test]
fn csv_format_matches_clickhouse_csv_with_names() {
    let output = run_with_stdin(
        "CREATE TABLE notes (id Int64, label String, active Bool, detail Nullable(String))\n\
         INSERT INTO notes VALUES (1, 'hello, world', true, 'quote: \"yes\"'), (2, '\\\\N', false, NULL)\n\
         SELECT id, label, active, detail FROM notes\n",
        &["--format", "csv"],
    );

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "\"id\",\"label\",\"active\",\"detail\"\n\
         1,\"hello, world\",true,\"quote: \"\"yes\"\"\"\n\
         2,\"\\N\",false,\\N\n"
    );
}

#[test]
fn format_accepts_equals_syntax_and_clickhouse_name() {
    let output = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args([
            "--format=CSVWithNames",
            "-e",
            "CREATE TABLE values_table (value Int64)",
            "-e",
            "INSERT INTO values_table VALUES (7)",
            "-e",
            "SELECT value FROM values_table",
        ])
        .output()
        .unwrap();

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "\"value\"\n7\n");
}

#[test]
fn invalid_and_missing_formats_are_rejected() {
    let cases = [
        (
            ["--format"].as_slice(),
            "--format requires an output format",
        ),
        (
            ["--format", "json"].as_slice(),
            "unknown output format \"json\"",
        ),
    ];
    for (arguments, expected_error) in cases {
        let output = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
            .args(arguments)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains(expected_error)
        );
    }
}
