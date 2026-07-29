use std::io::Write;
use std::process::{Command, Stdio};

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
        "[{\"name\":\"a\",\"n\":1},{\"name\":\"b\",\"n\":2}]\n"
    );
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(stderr.contains("CREATE TABLE"));
    assert!(stderr.contains("INSERT 2"));
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
