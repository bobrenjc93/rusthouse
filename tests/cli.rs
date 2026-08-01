use std::fs;
use std::path::PathBuf;
use std::process::Command;
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
    lock.push(".lock");
    let _ = fs::remove_file(PathBuf::from(lock));
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
