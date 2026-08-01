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
    lock.push(".rusthouse-lock");
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

#[test]
fn import_subcommand_persists_csv_and_rejects_late_errors_atomically() {
    let path = database_path();
    let input = path.with_extension("csv");
    let ndjson_input = path.with_extension("ndjson");
    let bad_input = path.with_extension("bad.csv");
    fs::write(&input, "id,label\n1,first\n2,second\n").unwrap();
    fs::write(&ndjson_input, "{\"label\":\"third\",\"id\":3}\n").unwrap();
    fs::write(&bad_input, "id,label\n4,valid\ninvalid,late\n").unwrap();

    let create = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .arg("--database")
        .arg(&path)
        .args(["-e", "CREATE TABLE imported (id Int64, label String)"])
        .output()
        .unwrap();
    assert!(create.status.success(), "{:?}", create.stderr);

    let imported = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args(["import", "csv", "--database"])
        .arg(&path)
        .arg("imported")
        .arg(&input)
        .output()
        .unwrap();
    assert!(imported.status.success(), "{:?}", imported.stderr);
    assert_eq!(String::from_utf8(imported.stdout).unwrap(), "IMPORT 2\n");

    let imported = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args(["import", "--format", "ndjson", "--database"])
        .arg(&path)
        .args(["--table", "imported", "--input"])
        .arg(&ndjson_input)
        .output()
        .unwrap();
    assert!(imported.status.success(), "{:?}", imported.stderr);
    assert_eq!(String::from_utf8(imported.stdout).unwrap(), "IMPORT 1\n");

    let failed = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args(["import", "csv", "--database"])
        .arg(&path)
        .arg("imported")
        .arg(&bad_input)
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert!(String::from_utf8(failed.stderr).unwrap().contains("row 2"));

    let selected = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .arg("--database")
        .arg(&path)
        .args(["-e", "SELECT * FROM imported"])
        .output()
        .unwrap();
    assert!(selected.status.success(), "{:?}", selected.stderr);
    let stdout = String::from_utf8(selected.stdout).unwrap();
    assert!(stdout.contains("1\tfirst\n2\tsecond\n3\tthird\n3 row(s)"));
    assert!(!stdout.contains("4\tvalid"));

    let _ = fs::remove_file(input);
    let _ = fs::remove_file(ndjson_input);
    let _ = fs::remove_file(bad_input);
    remove_database(&path);
}
