use std::ffi::OsStr;
#[cfg(unix)]
use std::ffi::OsString;
use std::fs;
use std::io::{Cursor, Write};
#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};

use rusthouse::cli::{
    BatchError, MAX_BATCH_BYTES, MAX_BATCH_STATEMENTS, MAX_STATEMENT_BYTES, execute_batch,
};
use rusthouse::{
    Catalog, CatalogError, CatalogLimits, DEFAULT_MAX_TABLES, GroupedCountError,
    MAX_AGGREGATE_RESULT_BYTES, SelectParseLimits, Value,
};

const BINARY: &str = env!("CARGO_BIN_EXE_rusthouse");
static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(test_name: &str) -> Self {
        let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("cli-tests")
            .join(format!("{test_name}-{}-{sequence}", std::process::id()));
        fs::create_dir_all(&path).expect("create test directory");
        Self(path)
    }

    fn snapshot(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("remove test directory");
    }
}

fn run(arguments: &[&str], input: &[u8]) -> Output {
    run_command(arguments.iter().copied(), input)
}

#[cfg(unix)]
fn run_os(arguments: &[&OsStr], input: &[u8]) -> Output {
    run_command(arguments.iter().copied(), input)
}

fn run_command<I, S>(arguments: I, input: &[u8]) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut child = Command::new(BINARY)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start rusthouse");

    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(input)
        .expect("write stdin");
    child.wait_with_output().expect("wait for rusthouse")
}

fn run_with_closed_stdout(arguments: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(BINARY)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start rusthouse");

    drop(child.stdout.take().expect("piped stdout"));
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(input)
        .expect("write stdin");
    child.wait_with_output().expect("wait for rusthouse")
}

#[test]
fn help_describes_the_bounded_batch_contract() {
    for argument in ["--help", "-h"] {
        let output = run(&[argument], b"");

        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains(
            "Usage: rusthouse [--format csv] [--load-table NAME=PATH] [--save-table NAME=PATH]"
        ));
        assert!(stdout.contains("CREATE TABLE, INSERT INTO ... VALUES, and SELECT"));
        assert!(stdout.contains("--format csv"));
        assert!(stdout.contains("--load-table NAME=PATH"));
        assert!(stdout.contains("--save-table NAME=PATH"));
        assert!(stdout.contains("1048576 bytes per statement"));
        assert!(stdout.contains("67108864 bytes per snapshot payload"));
        assert!(stdout.contains("4  unsupported statement"));
        assert!(stdout.contains("6  stdout write error"));
        assert_eq!(stdout.matches("Exit codes:").count(), 1);
    }
}

#[test]
fn rejects_arguments_with_the_usage_exit_code() {
    for arguments in [
        &["--unknown"][..],
        &["--format"][..],
        &["--format", "json"][..],
        &["--format", "CSV"][..],
        &["--format", "csv", "extra"][..],
        &["--format", "csv", "--format", "csv"][..],
        &["--load-table"][..],
        &["--load-table", "events"][..],
        &["--load-table", "=state.snapshot"][..],
        &["--save-table", "events="][..],
        &[
            "--load-table",
            "events=first.snapshot",
            "--load-table",
            "other=second.snapshot",
        ][..],
        &[
            "--save-table",
            "events=first.snapshot",
            "--save-table",
            "other=second.snapshot",
        ][..],
    ] {
        let output = run(arguments, b"");

        assert_eq!(output.status.code(), Some(2), "{arguments:?}");
        assert!(output.stdout.is_empty());
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            "rusthouse: invalid arguments; try 'rusthouse --help'\n"
        );
    }
}

#[test]
fn saves_reopens_and_selects_one_table_across_processes() {
    let directory = TestDirectory::new("save-reopen-select");
    let snapshot = directory.snapshot("events.snapshot");
    let save = format!("Events={}", snapshot.display());
    let saved = run(
        &["--save-table", &save],
        b"CREATE TABLE Events (id Int64, active Bool, label String)\n\
          INSERT INTO events VALUES (1, true, 'first'), (2, false, 'second')\n",
    );

    assert_eq!(saved.status.code(), Some(0));
    assert!(saved.stdout.is_empty());
    assert!(saved.stderr.is_empty());
    assert!(snapshot.exists());

    let load = format!("reopened={}", snapshot.display());
    let reopened = run(
        &["--format", "csv", "--load-table", &load],
        b"SELECT label, id FROM reopened WHERE active = true\n",
    );

    assert_eq!(reopened.status.code(), Some(0));
    assert!(reopened.stderr.is_empty());
    assert_eq!(reopened.stdout, b"\"label\",\"id\"\n\"first\",1\n");
}

#[test]
fn aggregates_or_groups_over_a_reopened_table() {
    let directory = TestDirectory::new("snapshot-aggregate-or-groups");
    let snapshot = directory.snapshot("events.snapshot");
    let save = format!("events={}", snapshot.display());
    let saved = run(
        &["--save-table", &save],
        b"CREATE TABLE events (id Int64, score Float64, active Bool, label String)\n\
          INSERT INTO events VALUES (1, 1.5, true, 'west'), (2, 4.0, false, 'east'), (3, 9.5, true, 'north'), (4, 2.0, true, 'south')\n",
    );
    assert_eq!(saved.status.code(), Some(0));
    assert!(saved.stdout.is_empty());
    assert!(saved.stderr.is_empty());

    let load = format!("restored={}", snapshot.display());
    let reopened = run(
        &["--load-table", &load],
        b"SELECT COUNT(*) AS matches, SUM(id) AS total, AVG(score) AS mean, MIN(label) AS first, MAX(active) AS any_active FROM restored WHERE (active = true AND score >= 2.0) OR (label = 'east' AND id >= 2) OR id = 3\n",
    );

    assert_eq!(reopened.status.code(), Some(0));
    assert!(reopened.stderr.is_empty());
    assert_eq!(
        reopened.stdout,
        b"\"matches\",\"total\",\"mean\",\"first\",\"any_active\"\n3,9,5.166666666666667,\"east\",true\n"
    );
}

#[test]
fn concurrent_saves_publish_one_complete_snapshot() {
    const WRITERS: usize = 16;
    const STRING_BYTES: usize = 256 * 1024;

    let directory = TestDirectory::new("concurrent-saves");
    let snapshot = directory.snapshot("events.snapshot");
    let mapping = format!("events={}", snapshot.display());
    let barrier = Arc::new(Barrier::new(WRITERS));
    let writers = (0..WRITERS)
        .map(|index| {
            let barrier = Arc::clone(&barrier);
            let mapping = mapping.clone();
            std::thread::spawn(move || {
                let label = format!("writer-{index}-{}", "x".repeat(STRING_BYTES));
                let input = format!(
                    "CREATE TABLE events (id Int64, label String)\n\
                     INSERT INTO events VALUES ({index}, '{label}')\n"
                );
                barrier.wait();
                run(&["--save-table", &mapping], input.as_bytes())
            })
        })
        .collect::<Vec<_>>();

    for writer in writers {
        let output = writer.join().expect("join snapshot writer");
        assert_eq!(
            output.status.code(),
            Some(0),
            "concurrent save failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let reopened = run(&["--load-table", &mapping], b"SELECT id FROM events\n");
    assert_eq!(reopened.status.code(), Some(0));
    assert!(reopened.stderr.is_empty());
    let stdout = String::from_utf8(reopened.stdout).unwrap();
    let id = stdout.lines().nth(1).unwrap().parse::<usize>().unwrap();
    assert!(id < WRITERS);
}

#[test]
fn rejects_sidecar_paths_without_mutating_the_primary_snapshot() {
    let directory = TestDirectory::new("reserved-sidecars");
    let primary = directory.snapshot("primary.snapshot");
    let temporary = directory.snapshot(".primary.snapshot.tmp");
    let lock = directory.snapshot(".primary.snapshot.lock");
    let primary_mapping = format!("events={}", primary.display());
    let temporary_mapping = format!("events={}", temporary.display());
    let lock_mapping = format!("events={}", lock.display());

    let primary_save = run(
        &["--save-table", &primary_mapping],
        b"CREATE TABLE events (id Int64)\nINSERT INTO events VALUES (7)\n",
    );
    assert_eq!(primary_save.status.code(), Some(0));

    for mapping in [&temporary_mapping, &lock_mapping] {
        let collision = run(
            &["--save-table", mapping],
            b"CREATE TABLE events (id Int64)\nINSERT INTO events VALUES (99)\n",
        );
        assert_eq!(collision.status.code(), Some(1));
        assert!(collision.stdout.is_empty());
        assert!(
            String::from_utf8(collision.stderr)
                .unwrap()
                .contains("is reserved for internal writer state")
        );
    }

    let reopened = run(
        &["--load-table", &primary_mapping],
        b"SELECT id FROM events\n",
    );
    assert_eq!(reopened.status.code(), Some(0));
    assert!(reopened.stderr.is_empty());
    assert_eq!(reopened.stdout, b"\"id\"\n7\n");
}

#[cfg(unix)]
#[test]
fn preserves_non_utf8_snapshot_paths_during_argument_parsing() {
    let directory = TestDirectory::new("non-utf8-path");
    let mut snapshot_bytes = directory.0.as_os_str().as_bytes().to_vec();
    snapshot_bytes.extend_from_slice(b"/events-\xff.snapshot");
    let snapshot = PathBuf::from(OsString::from_vec(snapshot_bytes));

    let mut mapping_bytes = b"events=".to_vec();
    mapping_bytes.extend_from_slice(snapshot.as_os_str().as_bytes());
    let mapping = OsString::from_vec(mapping_bytes);

    let output = run_os(
        &[OsStr::new("--load-table"), mapping.as_os_str()],
        b"SELECT id FROM events\n",
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "non-UTF-8 path was rejected as CLI usage: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .starts_with("rusthouse: could not load table `events` from ")
    );
}

#[cfg(target_os = "linux")]
#[test]
fn saves_and_reopens_a_non_utf8_snapshot_path() {
    let directory = TestDirectory::new("non-utf8-round-trip");
    let mut snapshot_bytes = directory.0.as_os_str().as_bytes().to_vec();
    snapshot_bytes.extend_from_slice(b"/events-\xff.snapshot");
    let snapshot = PathBuf::from(OsString::from_vec(snapshot_bytes));

    let mut mapping_bytes = b"events=".to_vec();
    mapping_bytes.extend_from_slice(snapshot.as_os_str().as_bytes());
    let mapping = OsString::from_vec(mapping_bytes);

    let saved = run_os(
        &[OsStr::new("--save-table"), mapping.as_os_str()],
        b"CREATE TABLE events (id Int64)\nINSERT INTO events VALUES (7)\n",
    );
    assert_eq!(saved.status.code(), Some(0));
    assert!(saved.stderr.is_empty());
    assert!(snapshot.exists());

    let reopened = run_os(
        &[OsStr::new("--load-table"), mapping.as_os_str()],
        b"SELECT id FROM events\n",
    );
    assert_eq!(reopened.status.code(), Some(0));
    assert!(reopened.stderr.is_empty());
    assert_eq!(reopened.stdout, b"\"id\"\n7\n");
}

#[test]
fn rejects_a_corrupt_snapshot_before_processing_stdin() {
    let directory = TestDirectory::new("corrupt-load");
    let snapshot = directory.snapshot("corrupt.snapshot");
    let mapping = format!("events={}", snapshot.display());
    let saved = run(
        &["--save-table", &mapping],
        b"CREATE TABLE events (id Int64)\nINSERT INTO events VALUES (1)\n",
    );
    assert_eq!(saved.status.code(), Some(0));

    let mut bytes = fs::read(&snapshot).expect("read snapshot");
    *bytes.last_mut().expect("nonempty snapshot") ^= 0xff;
    fs::write(&snapshot, bytes).expect("corrupt snapshot");

    let output = run(
        &["--load-table", &mapping],
        b"CREATE TABLE ignored (id Int64)\nSELECT id FROM ignored\n",
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.starts_with(&format!(
        "rusthouse: could not load table `events` from {}: ",
        snapshot.display()
    )));
    assert!(stderr.contains("snapshot checksum mismatch"));
}

#[test]
fn does_not_replace_a_snapshot_when_the_batch_fails() {
    let directory = TestDirectory::new("failed-batch");
    let snapshot = directory.snapshot("events.snapshot");
    let mapping = format!("events={}", snapshot.display());
    let initial = run(
        &["--save-table", &mapping],
        b"CREATE TABLE events (id Int64)\nINSERT INTO events VALUES (1)\n",
    );
    assert_eq!(initial.status.code(), Some(0));

    let failed = run(
        &["--load-table", &mapping, "--save-table", &mapping],
        b"INSERT INTO events VALUES (2)\nINSERT INTO events VALUES ('wrong')\n",
    );
    assert_eq!(failed.status.code(), Some(1));
    assert!(failed.stdout.is_empty());
    assert!(
        String::from_utf8(failed.stderr)
            .unwrap()
            .contains("execution error on line 2")
    );

    let reopened = run(
        &["--load-table", &mapping],
        b"SELECT id FROM events ORDER BY id\n",
    );
    assert_eq!(reopened.status.code(), Some(0));
    assert!(reopened.stderr.is_empty());
    assert_eq!(reopened.stdout, b"\"id\"\n1\n");
}

#[test]
fn executes_mixed_create_and_insert_lines_in_one_catalog() {
    let output = run(
        &[],
        b"\n  \r\nCREATE TABLE Events (id Int64, ratio Float64, active Bool, label String)\n\
          INSERT INTO events VALUES (1, 1.5, true, 'first')\n\
          insert into EVENTS values (2, -3.25, FALSE, 'second')\n",
    );

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn reports_malformed_supported_statements_with_line_numbers() {
    let output = run(
        &[],
        b"\nCREATE TABLE events (id Int64)\nINSERT INTO events VALUES ('wrong')\n",
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "rusthouse: execution error on line 3: could not insert into table `events`: batch row 0, column 0 (`id`) has type String; expected Int64\n"
    );
}

#[test]
fn rejects_non_utf8_and_unsupported_statements_deterministically() {
    let invalid_utf8 = run(
        &[],
        &[b'C', b'R', b'E', b'A', b'T', b'E', b' ', 0xff, b'\n'],
    );
    assert_eq!(invalid_utf8.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(invalid_utf8.stderr).unwrap(),
        "rusthouse: input error on line 1: statement is not valid UTF-8\n"
    );

    let unsupported = run(&[], b"DELETE FROM events\n");
    assert_eq!(unsupported.status.code(), Some(4));
    assert!(unsupported.stdout.is_empty());
    assert_eq!(
        String::from_utf8(unsupported.stderr).unwrap(),
        "rusthouse: unsupported statement on line 1: expected CREATE TABLE, INSERT INTO, or SELECT\n"
    );
}

#[test]
fn writes_each_select_with_projected_columns_and_filtered_rows() {
    let output = run(
        &["--format", "csv"],
        b"CREATE TABLE events (id Int64, active Bool, label String)\n\
          INSERT INTO events VALUES (1, true, 'first'), (2, false, 'second'), (3, true, 'third')\n\
          SELECT label, id, label FROM events WHERE active = true\n\
          SELECT active FROM events WHERE id >= 2\n",
    );

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    assert_eq!(
        output.stdout,
        concat!(
            "\"label\",\"id\",\"label\"\n",
            "\"first\",1,\"first\"\n",
            "\"third\",3,\"third\"\n",
            "\"active\"\n",
            "false\n",
            "true\n",
        )
        .as_bytes()
    );
}

#[test]
fn writes_only_projected_headers_for_empty_results() {
    let output = run(
        &["--format", "csv"],
        b"CREATE TABLE events (id Int64, label String)\n\
          INSERT INTO events VALUES (1, 'first')\n\
          SELECT label, id FROM events WHERE id > 10\n",
    );

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    assert_eq!(output.stdout, b"\"label\",\"id\"\n");
}

#[test]
fn writes_count_rows_for_all_filtered_and_empty_tables() {
    let output = run(
        &["--format", "csv"],
        b"CREATE TABLE events (id Int64, active Bool)\n\
          INSERT INTO events VALUES (1, true), (2, false), (3, true)\n\
          CREATE TABLE empty (id Int64)\n\
          SELECT COUNT(*) FROM events\n\
          SELECT COUNT(*) AS active_count FROM events WHERE active = true\n\
          SELECT COUNT(*) AS no_matches FROM events WHERE id > 10\n\
          SELECT COUNT(*) AS empty_count FROM empty\n",
    );

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    assert_eq!(
        output.stdout,
        concat!(
            "\"count()\"\n",
            "3\n",
            "\"active_count\"\n",
            "2\n",
            "\"no_matches\"\n",
            "0\n",
            "\"empty_count\"\n",
            "0\n",
        )
        .as_bytes()
    );
}

#[test]
fn writes_filtered_distinct_counts_with_aliases_and_empty_inputs() {
    let output = run(
        &["--format", "csv"],
        b"CREATE TABLE events (id Int64, score Float64, active Bool, label String)\n\
          INSERT INTO events VALUES (1, 1.5, true, 'east'), (1, 1.5, true, 'east'), (2, 2.5, false, 'west'), (3, 3.5, true, 'north')\n\
          CREATE TABLE empty (value String)\n\
          SELECT COUNT(DISTINCT id) AS ids, COUNT(DISTINCT score), COUNT(DISTINCT active), COUNT(DISTINCT label) AS labels FROM events WHERE active = true\n\
          SELECT COUNT(DISTINCT value) FROM empty\n",
    );

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    assert_eq!(
        output.stdout,
        concat!(
            "\"ids\",\"count(distinct score)\",\"count(distinct active)\",\"labels\"\n",
            "2,2,1,2\n",
            "\"count(distinct value)\"\n",
            "0\n",
        )
        .as_bytes()
    );
}

#[test]
fn writes_one_csv_row_for_a_filtered_aggregate_list() {
    let output = run(
        &["--format", "csv"],
        b"CREATE TABLE events (id Int64, score Float64, active Bool, label String)\n\
          INSERT INTO events VALUES (1, -2.0, true, 'first'), (2, 0.0, false, 'second'), (3, 4.0, true, 'third')\n\
          SELECT COUNT(*), SUM(id) AS total_id, AVG(score), MIN(label) AS first_label, MAX(active) FROM events WHERE active = true\n",
    );

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    assert_eq!(
        output.stdout,
        concat!(
            "\"count()\",\"total_id\",\"avg(score)\",\"first_label\",\"max(active)\"\n",
            "2,4,1,\"first\",true\n",
        )
        .as_bytes()
    );
}

#[test]
fn writes_grouped_counts_for_every_type_with_filtering_and_empty_results() {
    let output = run(
        &["--format", "csv"],
        b"CREATE TABLE groups (integer Int64, float Float64, boolean Bool, text String)\n\
          INSERT INTO groups VALUES (4, 2.5, true, 'pear'), (-2, -1.0, false, 'apple'), (4, 2.5, false, 'pear'), (0, 9.0, true, 'banana'), (-2, -1.0, false, 'apple')\n\
          SELECT integer, COUNT(*) FROM groups GROUP BY integer\n\
          SELECT float, COUNT(*) AS rows FROM groups GROUP BY float\n\
          SELECT boolean, COUNT(*) FROM groups GROUP BY boolean\n\
          SELECT text, COUNT(*) FROM groups WHERE boolean = true GROUP BY text\n\
          SELECT text, COUNT(*) AS matches FROM groups WHERE integer > 100 GROUP BY text\n",
    );

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    assert_eq!(
        output.stdout,
        concat!(
            "\"integer\",\"count()\"\n-2,2\n0,1\n4,2\n",
            "\"float\",\"rows\"\n-1,2\n2.5,2\n9,1\n",
            "\"boolean\",\"count()\"\nfalse,3\ntrue,2\n",
            "\"text\",\"count()\"\n\"banana\",1\n\"pear\",1\n",
            "\"text\",\"matches\"\n",
        )
        .as_bytes()
    );
}

#[test]
fn reports_the_group_limit_as_a_cli_resource_limit() {
    let limits = CatalogLimits::default().with_max_groups_per_query(1);
    let mut catalog = Catalog::with_limits(limits);
    catalog
        .execute_create("CREATE TABLE groups (key String)")
        .unwrap();
    catalog
        .execute_insert("INSERT INTO groups VALUES ('a'), ('b')")
        .unwrap();

    let error = execute_batch(
        Cursor::new(b"SELECT key, COUNT(*) FROM groups GROUP BY key\n"),
        &mut catalog,
    )
    .unwrap_err();
    assert_eq!(error.exit_code(), 3);
    assert!(matches!(
        error,
        BatchError::ExecutionLimit {
            line: 1,
            source: CatalogError::TableGrouping {
                source: GroupedCountError::GroupLimitExceeded { limit: 1, .. },
                ..
            },
        }
    ));
}

#[test]
fn reports_the_grouped_string_byte_limit_as_a_cli_resource_limit() {
    let limits = CatalogLimits::default().with_max_grouped_result_bytes(3);
    let mut catalog = Catalog::with_limits(limits);
    catalog
        .execute_create("CREATE TABLE groups (key String)")
        .unwrap();
    catalog
        .execute_insert("INSERT INTO groups VALUES ('abc'), ('de'), ('abc')")
        .unwrap();

    execute_batch(
        Cursor::new(b"SELECT key, COUNT(*) FROM groups WHERE key = 'abc' GROUP BY key\n"),
        &mut catalog,
    )
    .unwrap();

    let error = execute_batch(
        Cursor::new(b"SELECT key, COUNT(*) FROM groups GROUP BY key\n"),
        &mut catalog,
    )
    .unwrap_err();
    assert_eq!(error.exit_code(), 3);
    assert!(matches!(
        error,
        BatchError::ExecutionLimit {
            line: 1,
            source: CatalogError::TableGrouping {
                source: GroupedCountError::StringResultTooLarge {
                    limit: 3,
                    required: 5,
                    ..
                },
                ..
            },
        }
    ));
}

#[test]
fn escapes_select_strings_as_csv() {
    let output = run(
        &["--format", "csv"],
        b"CREATE TABLE messages (id Int64, body String)\n\
          INSERT INTO messages VALUES (7, 'comma, \"quote\" and apostrophe ''')\n\
          SELECT body, id FROM messages\n",
    );

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    assert_eq!(
        output.stdout,
        b"\"body\",\"id\"\n\"comma, \"\"quote\"\" and apostrophe '\",7\n"
    );
}

#[test]
fn reports_stdout_failures_against_the_select_line() {
    let output = run_with_closed_stdout(
        &["--format", "csv"],
        b"CREATE TABLE events (id Int64)\n\
          INSERT INTO events VALUES (1)\n\
          SELECT id FROM events\n",
    );

    assert_eq!(output.status.code(), Some(6));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "rusthouse: output error on line 3: could not write SELECT result\n"
    );
}

#[test]
fn enforces_the_per_statement_byte_bound() {
    let prefix = b"CREATE TABLE exact (id Int64)";
    let mut exact = Vec::with_capacity(MAX_STATEMENT_BYTES + 1);
    exact.extend_from_slice(prefix);
    exact.resize(MAX_STATEMENT_BYTES, b' ');
    exact.push(b'\n');

    let accepted = run(&[], &exact);
    assert_eq!(accepted.status.code(), Some(0));
    assert!(accepted.stderr.is_empty());

    exact.insert(MAX_STATEMENT_BYTES, b' ');
    let rejected = run(&[], &exact);
    assert_eq!(rejected.status.code(), Some(3));
    assert_eq!(
        String::from_utf8(rejected.stderr).unwrap(),
        format!(
            "rusthouse: input limit exceeded on line 1: statement exceeds {MAX_STATEMENT_BYTES} bytes\n"
        )
    );
}

#[test]
fn enforces_the_nonempty_statement_count_bound() {
    let mut input = String::from("CREATE TABLE bounded (id Int64)\n");
    for _ in 1..MAX_BATCH_STATEMENTS {
        input.push_str("INSERT INTO bounded VALUES (1)\n");
    }
    input.push_str("INSERT INTO bounded VALUES (2)\n");

    let output = run(&[], input.as_bytes());

    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!(
            "rusthouse: input limit exceeded on line {}: batch exceeds {MAX_BATCH_STATEMENTS} statements\n",
            MAX_BATCH_STATEMENTS + 1
        )
    );
}

#[test]
fn reports_catalog_capacity_as_a_limit() {
    let mut input = String::new();
    for table in 0..=DEFAULT_MAX_TABLES {
        input.push_str(&format!("CREATE TABLE t{table} (id Int64)\n"));
    }

    let output = run(&[], input.as_bytes());

    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!(
            "rusthouse: resource limit exceeded on line {}: catalog table count exceeds limit of {DEFAULT_MAX_TABLES}\n",
            DEFAULT_MAX_TABLES + 1
        )
    );
}

#[test]
fn reports_excessive_order_keys_as_a_resource_limit() {
    let limit = SelectParseLimits::DEFAULT_MAX_ORDER_KEYS;
    let mut input = String::from("SELECT id FROM events ORDER BY ");
    for key in 0..=limit {
        if key != 0 {
            input.push_str(", ");
        }
        input.push_str("id");
    }
    let excess_key_position = input.rfind("id").unwrap();
    input.push('\n');

    let output = run(&[], input.as_bytes());

    assert_eq!(output.status.code(), Some(3));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!(
            "rusthouse: resource limit exceeded on line 1: SQL parse error at byte \
             {excess_key_position}: order key count exceeds limit of {limit}\n"
        )
    );
}

#[test]
fn reports_aggregate_result_bytes_as_a_limit() {
    const VALUE_BYTES: usize = MAX_AGGREGATE_RESULT_BYTES / 2 + 1;
    const REQUIRED_BYTES: usize = VALUE_BYTES * 2;

    let mut catalog = Catalog::new();
    catalog
        .execute_create("CREATE TABLE strings (value String)")
        .unwrap();
    catalog
        .table_mut("strings")
        .unwrap()
        .insert_batch([vec![Value::String("x".repeat(VALUE_BYTES))]])
        .unwrap();

    let error = execute_batch(
        Cursor::new(b"SELECT MIN(value), MAX(value) FROM strings\n"),
        &mut catalog,
    )
    .unwrap_err();

    assert_eq!(error.exit_code(), 3);
    assert!(matches!(
        error,
        BatchError::ExecutionLimit {
            line: 1,
            source: CatalogError::AggregateResultTooLarge {
                limit: MAX_AGGREGATE_RESULT_BYTES,
                required: REQUIRED_BYTES,
                ..
            },
        }
    ));
}

#[test]
fn enforces_the_total_stdin_byte_bound() {
    let mut input = Vec::with_capacity(MAX_BATCH_BYTES + MAX_STATEMENT_BYTES);
    while input.len() <= MAX_BATCH_BYTES {
        input.resize(input.len() + MAX_STATEMENT_BYTES, b' ');
        input.push(b'\n');
    }

    let output = run(&[], &input);

    assert_eq!(output.status.code(), Some(3));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!(
            "rusthouse: input limit exceeded on line 16: stdin exceeds {MAX_BATCH_BYTES} bytes\n"
        )
    );

    let mut input = Cursor::new(input);
    let error = execute_batch(&mut input, &mut Catalog::new()).unwrap_err();
    assert!(matches!(error, BatchError::BatchTooLarge { .. }));
    assert_eq!(input.position(), MAX_BATCH_BYTES as u64 + 1);
}
