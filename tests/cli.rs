use std::io::Write;
use std::process::{Command, Stdio};

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_rusthouse"))
}

#[test]
fn help_is_available_without_startup_text() {
    let output = binary().arg("--help").output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("RustHouse in-memory analytical SQL engine\n"));
    assert!(stdout.contains("--format <FORMAT>"));
    assert!(!stdout.contains("warming up"));
    assert!(output.stderr.is_empty());
}

#[test]
fn multi_statement_stdin_emits_escaped_csv_with_headers() {
    let mut child = binary()
        .args(["--format", "csv"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            b"CREATE TABLE `arbitrary ledger` (`account,label` String, amount Float64, open Bool);
              INSERT INTO `arbitrary ledger` VALUES ('A,\"quoted\"', 2.5, true), ('B', 1, false);
              SELECT `account,label` AS `account,name`, amount, open FROM `arbitrary ledger` ORDER BY amount DESC;",
        )
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "\"account,name\",amount,open\n\"A,\"\"quoted\"\"\",2.5,true\nB,1,false\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn closed_output_pipe_is_not_reported_as_a_failure() {
    let values = (0..20_000)
        .map(|value| format!("({value}, 'a fairly long value {value}')"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "CREATE TABLE pipe_rows (id Int64, label String);
         INSERT INTO pipe_rows VALUES {values};
         SELECT * FROM pipe_rows;"
    );
    let mut child = binary()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(sql.as_bytes())
        .unwrap();
    drop(child.stdout.take());
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
}
