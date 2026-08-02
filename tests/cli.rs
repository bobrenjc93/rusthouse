use std::io::Write;
use std::process::{Command, Output, Stdio};

use rusthouse::MAX_QUERY_BYTES;

#[cfg(unix)]
fn closed_output() -> Stdio {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;

    let (output, peer) = UnixStream::pair().unwrap();
    drop(peer);
    Stdio::from(OwnedFd::from(output))
}

fn run(arguments: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn help_succeeds() {
    let output = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .arg("--help")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("Usage: rusthouse")
    );
    assert!(output.stderr.is_empty());
}

#[cfg(unix)]
#[test]
fn help_reports_closed_stdout_without_panicking() {
    let output = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .arg("--help")
        .stdout(closed_output())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr.contains("failed to write standard output"),
        "{stderr}"
    );
    assert!(!stderr.contains("panicked at"), "{stderr}");
}

#[cfg(unix)]
#[test]
fn errors_preserve_exit_codes_when_stderr_is_closed() {
    for (arguments, expected_code) in [(&[][..], 1), (&["--unknown"][..], 2)] {
        let output = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(closed_output())
            .output()
            .unwrap();

        assert_eq!(output.status.code(), Some(expected_code));
    }
}

#[test]
fn csv_format_reads_to_eof_and_escapes_header_and_row() {
    let sql = b"SELECT 'Ada said ''hello'',\nagain' AS \"greeting, \"\"quoted\"\"\"";
    let output = run(&["--format", "csv"], sql);

    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "\"greeting, \"\"quoted\"\"\"\n\"Ada said 'hello',\nagain\"\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn csv_serializes_each_literal_type() {
    for (sql, expected) in [
        ("SELECT -8 AS value", "value\n-8\n"),
        ("SELECT 3.25 AS value", "value\n3.25\n"),
        ("SELECT TRUE AS value", "value\ntrue\n"),
        ("SELECT 'text' AS value", "value\ntext\n"),
    ] {
        let output = run(&["--format=csv"], sql.as_bytes());
        assert!(output.status.success(), "{:?}", output.stderr);
        assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
    }
}

#[test]
fn malformed_sql_fails_without_stdout() {
    let output = run(&["--format", "csv"], b"SELECT 1");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("invalid SQL")
    );
}

#[test]
fn unsupported_options_and_formats_fail() {
    for arguments in [&["--unknown"][..], &["--format", "json"][..]] {
        let output = run(arguments, b"SELECT 1 AS value");
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains("unsupported")
        );
    }
}

#[test]
fn stdin_is_bounded_by_bytes() {
    let mut maximum = b"SELECT 1 AS value".to_vec();
    maximum.resize(MAX_QUERY_BYTES, b' ');
    let output = run(&["--format", "csv"], &maximum);
    assert!(output.status.success(), "{:?}", output.stderr);

    maximum.push(b' ');
    let output = run(&["--format", "csv"], &maximum);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("input limit")
    );
}
