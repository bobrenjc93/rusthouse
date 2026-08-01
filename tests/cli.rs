use std::io::Write;
use std::process::{Command, Stdio};

fn run(input: &str, args: &[&str]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args(args)
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
fn stdin_to_csv_is_header_bearing_end_to_end() {
    let output = run(
        "CREATE TABLE metrics (name String, value Int64);\n\
         INSERT INTO metrics VALUES ('b', 2), ('a', 3), ('a', 4);\n\
         SELECT name, SUM(value) AS total FROM metrics\n\
         GROUP BY name ORDER BY total DESC, name LIMIT 2;",
        &["--format", "csv"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "name,total\na,7\nb,2\n"
    );
}

#[test]
fn cli_reports_sql_and_resource_boundaries() {
    let malformed = run("SELECT (", &["--format=csv"]);
    assert!(!malformed.status.success());
    assert!(String::from_utf8_lossy(&malformed.stderr).contains("SQL error"));

    let limited = run("SELECT 1;", &["--max-input-bytes", "5"]);
    assert!(!limited.status.success());
    assert!(String::from_utf8_lossy(&limited.stderr).contains("limit exceeded"));
}

#[cfg(unix)]
#[test]
fn closed_output_pipe_is_a_successful_exit() {
    let mut head = Command::new("head")
        .args(["-n", "1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let head_input = head.stdin.take().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args(["--format", "csv"])
        .stdin(Stdio::piped())
        .stdout(Stdio::from(head_input))
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let values = (0..20_000)
        .map(|value| format!("({value})"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "CREATE TABLE numbers (n Int64); INSERT INTO numbers VALUES {values}; SELECT n FROM numbers;"
    );
    child
        .stdin
        .take()
        .unwrap()
        .write_all(sql.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    head.wait().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
