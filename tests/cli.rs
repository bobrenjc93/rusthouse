use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_FILE_ID: AtomicU64 = AtomicU64::new(0);

fn run_with_stdin(arguments: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rusthouse"))
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn CLI");

    child
        .stdin
        .take()
        .expect("stdin pipe")
        .write_all(input.as_bytes())
        .expect("write CLI input");
    child.wait_with_output().expect("wait for CLI")
}

fn temp_sql_path() -> PathBuf {
    let id = TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("rusthouse-cli-{}-{id}.sql", std::process::id()))
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

#[test]
fn interactive_session_keeps_state_and_continues_after_sql_errors() {
    let output = run_with_stdin(
        &["--interactive", "--format", "csv"],
        "CREATE TABLE notes (\n\
             label String,\n\
             n Int64\n\
         );\n\
         INSERT INTO notes VALUES ('semi;colon', 1); -- not a boundary ;\n\
         SELECT missing FROM notes;\n\
         INSERT INTO notes VALUES ('after error', 2);\n\
         SELECT label, n FROM notes ORDER BY n;\n\
         \\q\n\
         SELECT label FROM notes;\n",
    );

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    assert_eq!(stdout, "label,n\nsemi;colon,1\nafter error,2\n");
    assert!(!stdout.contains("rusthouse>"));

    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(stderr.contains("rusthouse> "));
    assert!(stderr.contains("        -> "));
    assert!(stderr.contains("column 'missing' does not exist"));
    assert!(stderr.contains("INSERT 1"));
}

#[test]
fn interactive_commands_change_format_read_files_and_quit() {
    let path = temp_sql_path();
    fs::write(
        &path,
        "CREATE TABLE loaded (id Int64, label String);\n\
         INSERT INTO loaded VALUES (2, 'from;file'), (1, 'first')",
    )
    .expect("write SQL file");

    let input = format!(
        "\\read {}\n\
         \\format json\n\
         SELECT id FROM loaded ORDER BY id;\n\
         \\format invalid\n\
         \\format csv\n\
         SELECT label FROM loaded ORDER BY label;\n\
         \\q\n\
         SELECT id FROM loaded;\n",
        path.display()
    );
    let output = run_with_stdin(&["--interactive"], &input);
    fs::remove_file(&path).expect("remove SQL file");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    assert_eq!(
        stdout,
        "{\"results\":[{\"columns\":[{\"name\":\"id\",\"type\":\"Int64\"}],\"rows\":[[1],[2]]}]}\n\
         label\n\
         first\n\
         from;file\n"
    );
    assert!(!stdout.contains("rusthouse>"));

    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(stderr.contains("CREATE TABLE"));
    assert!(stderr.contains("INSERT 2"));
    assert!(stderr.contains("unknown output format 'invalid'"));
}
