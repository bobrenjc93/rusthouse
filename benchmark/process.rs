use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub const CLICKHOUSE_VERSION: &str = "26.7.1";
pub const CLICKHOUSE_SHA256: &str =
    "6611c5aadcfac188031fa0fdf2676ec311771f96654a62b918b146b60dd11075";

#[derive(Debug, Clone)]
pub struct EnginePaths {
    pub rusthouse: PathBuf,
    pub clickhouse: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ClickHouseIdentity {
    pub version_output: String,
    pub sha256: String,
}

#[derive(Debug, Clone)]
pub struct RustHouseIdentity {
    pub sha256: String,
}

#[derive(Debug)]
pub struct RustHouseSnapshot {
    directory: PathBuf,
    executable: PathBuf,
}

#[derive(Debug, Clone)]
pub struct BenchmarkIdentity {
    pub clickhouse: ClickHouseIdentity,
    pub rusthouse: RustHouseIdentity,
}

#[derive(Debug, Clone, Copy)]
pub enum Engine {
    RustHouse,
    ClickHouse,
}

#[derive(Debug)]
pub struct TimedOutput {
    pub stdout: String,
}

#[derive(Debug)]
pub struct TimedBatch {
    pub elapsed: Duration,
    pub query_repetitions: usize,
}

impl EnginePaths {
    pub fn validate(&self) -> Result<BenchmarkIdentity, String> {
        let rusthouse = validate_rusthouse(&self.rusthouse)?;
        let clickhouse = validate_clickhouse(&self.clickhouse)?;
        Ok(BenchmarkIdentity {
            clickhouse,
            rusthouse,
        })
    }

    pub fn execute_correctness(
        &self,
        engine: Engine,
        setup_sql: &str,
        query_sql: &str,
    ) -> Result<TimedOutput, String> {
        let batch = sql_batch(setup_sql, query_sql, 1)?;
        let (_, stdout) = self.execute_batch(engine, &batch, true)?;
        Ok(TimedOutput {
            stdout: stdout.expect("captured execution returns stdout"),
        })
    }

    pub fn execute_timed(
        &self,
        engine: Engine,
        setup_sql: &str,
        query_sql: &str,
        query_repetitions: usize,
    ) -> Result<TimedBatch, String> {
        let batch = sql_batch(setup_sql, query_sql, query_repetitions)?;
        let (elapsed, stdout) = self.execute_batch(engine, &batch, false)?;
        debug_assert!(stdout.is_none());
        Ok(TimedBatch {
            elapsed,
            query_repetitions,
        })
    }

    fn execute_batch(
        &self,
        engine: Engine,
        batch: &str,
        capture_stdout: bool,
    ) -> Result<(Duration, Option<String>), String> {
        let mut command = match engine {
            Engine::RustHouse => {
                let mut command = Command::new(&self.rusthouse);
                command.args(["--format", "csv"]);
                command
            }
            Engine::ClickHouse => {
                let mut command = Command::new(&self.clickhouse);
                command.args(["local", "--multiquery", "--output-format", "CSVWithNames"]);
                command
            }
        };
        command
            .stdin(Stdio::piped())
            .stdout(if capture_stdout {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stderr(Stdio::piped());

        let started = Instant::now();
        let mut child = command.spawn().map_err(|error| {
            format!(
                "could not start {} at '{}': {error}",
                engine.name(),
                engine.path(self).display()
            )
        })?;
        child
            .stdin
            .take()
            .ok_or_else(|| format!("{} stdin was not piped", engine.name()))?
            .write_all(batch.as_bytes())
            .map_err(|error| format!("could not write SQL to {}: {error}", engine.name()))?;
        let output = child
            .wait_with_output()
            .map_err(|error| format!("could not wait for {}: {error}", engine.name()))?;
        let elapsed = started.elapsed();

        if !output.status.success() {
            return Err(format!(
                "{} exited with {}: {}",
                engine.name(),
                output.status,
                summarize_stderr(&output.stderr)
            ));
        }
        let stdout =
            if capture_stdout {
                Some(String::from_utf8(output.stdout).map_err(|error| {
                    format!("{} emitted non-UTF-8 output: {error}", engine.name())
                })?)
            } else {
                None
            };
        Ok((elapsed, stdout))
    }
}

impl RustHouseSnapshot {
    pub fn create(source: &Path, label: &str) -> Result<Self, String> {
        let directory = create_snapshot_directory(label)?;
        let executable = directory.join(format!("rusthouse{}", std::env::consts::EXE_SUFFIX));
        let snapshot = Self {
            directory,
            executable,
        };
        fs::copy(source, &snapshot.executable).map_err(|error| {
            format!(
                "could not snapshot {label} RustHouse binary '{}' to '{}': {error}",
                source.display(),
                snapshot.executable.display()
            )
        })?;
        make_snapshot_read_only(&snapshot.executable).map_err(|error| {
            format!(
                "could not make {label} RustHouse snapshot '{}' read-only: {error}",
                snapshot.executable.display()
            )
        })?;
        seal_snapshot_directory(&snapshot.directory).map_err(|error| {
            format!(
                "could not seal {label} RustHouse snapshot directory '{}': {error}",
                snapshot.directory.display()
            )
        })?;
        Ok(snapshot)
    }

    pub fn path(&self) -> &Path {
        &self.executable
    }

    pub fn verify_unchanged(
        &self,
        identity: &RustHouseIdentity,
        label: &str,
    ) -> Result<(), String> {
        let observed = sha256(&self.executable, &format!("{label} RustHouse snapshot"))?;
        if observed != identity.sha256 {
            return Err(format!(
                "{label} RustHouse snapshot changed during the benchmark: expected SHA-256 {}, got {observed}",
                identity.sha256
            ));
        }
        Ok(())
    }
}

impl Drop for RustHouseSnapshot {
    fn drop(&mut self) {
        make_snapshot_directory_writable(&self.directory);
        make_snapshot_writable(&self.executable);
        let _ = fs::remove_file(&self.executable);
        let _ = fs::remove_dir(&self.directory);
    }
}

static SNAPSHOT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn create_snapshot_directory(label: &str) -> Result<PathBuf, String> {
    for _ in 0..100 {
        let sequence = SNAPSHOT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "rusthouse-benchmark-{}-{label}-{sequence}",
            std::process::id()
        ));
        match create_private_directory(&directory) {
            Ok(()) => return Ok(directory),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "could not create RustHouse snapshot directory '{}': {error}",
                    directory.display()
                ));
            }
        }
    }
    Err("could not allocate a unique RustHouse snapshot directory".to_owned())
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_directory(path: &Path) -> std::io::Result<()> {
    fs::create_dir(path)
}

#[cfg(unix)]
fn seal_snapshot_directory(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o500))
}

#[cfg(not(unix))]
fn seal_snapshot_directory(path: &Path) -> std::io::Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(path, permissions)
}

#[cfg(unix)]
fn make_snapshot_directory_writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn make_snapshot_directory_writable(path: &Path) {
    if let Ok(metadata) = fs::metadata(path) {
        let mut permissions = metadata.permissions();
        permissions.set_readonly(false);
        let _ = fs::set_permissions(path, permissions);
    }
}

#[cfg(unix)]
fn make_snapshot_read_only(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o500))
}

#[cfg(not(unix))]
fn make_snapshot_read_only(path: &Path) -> std::io::Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(path, permissions)
}

#[cfg(unix)]
fn make_snapshot_writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn make_snapshot_writable(path: &Path) {
    if let Ok(metadata) = fs::metadata(path) {
        let mut permissions = metadata.permissions();
        permissions.set_readonly(false);
        let _ = fs::set_permissions(path, permissions);
    }
}

fn sql_batch(setup_sql: &str, query_sql: &str, query_repetitions: usize) -> Result<String, String> {
    if query_repetitions == 0 {
        return Err("query repetition count must be positive".to_owned());
    }
    let query_bytes = query_sql
        .len()
        .checked_add(1)
        .and_then(|length| length.checked_mul(query_repetitions))
        .ok_or_else(|| "amplified SQL batch is too large".to_owned())?;
    let capacity = setup_sql
        .len()
        .checked_add(query_bytes)
        .ok_or_else(|| "amplified SQL batch is too large".to_owned())?;
    let mut batch = String::with_capacity(capacity);
    batch.push_str(setup_sql);
    for _ in 0..query_repetitions {
        batch.push_str(query_sql);
        batch.push('\n');
    }
    Ok(batch)
}

impl Engine {
    fn name(self) -> &'static str {
        match self {
            Self::RustHouse => "RustHouse",
            Self::ClickHouse => "ClickHouse Local",
        }
    }

    fn path(self, paths: &EnginePaths) -> &Path {
        match self {
            Self::RustHouse => &paths.rusthouse,
            Self::ClickHouse => &paths.clickhouse,
        }
    }
}

pub fn validate_rusthouse(path: &Path) -> Result<RustHouseIdentity, String> {
    let output = Command::new(path).arg("--help").output().map_err(|error| {
        format!(
            "could not execute RustHouse at '{}': {error}",
            path.display()
        )
    })?;
    if !output.status.success() {
        return Err(format!(
            "RustHouse validation failed with {}: {}",
            output.status,
            summarize_stderr(&output.stderr)
        ));
    }
    Ok(RustHouseIdentity {
        sha256: sha256(path, "RustHouse")?,
    })
}

fn validate_clickhouse(path: &Path) -> Result<ClickHouseIdentity, String> {
    let output = Command::new(path)
        .args(["local", "--version"])
        .output()
        .map_err(|error| {
            format!(
                "could not execute ClickHouse Local at '{}': {error}",
                path.display()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "ClickHouse version check failed with {}: {}",
            output.status,
            summarize_stderr(&output.stderr)
        ));
    }
    let version_output = String::from_utf8(output.stdout)
        .map_err(|error| format!("ClickHouse version output was not UTF-8: {error}"))?
        .trim()
        .to_owned();
    if !version_output.contains(CLICKHOUSE_VERSION) {
        return Err(format!(
            "unsupported ClickHouse version {version_output:?}; expected {CLICKHOUSE_VERSION}"
        ));
    }

    let sha256 = sha256(path, "ClickHouse")?;

    if sha256 != CLICKHOUSE_SHA256 {
        return Err(format!(
            "ClickHouse checksum mismatch: expected {CLICKHOUSE_SHA256}, got {sha256}"
        ));
    }

    Ok(ClickHouseIdentity {
        version_output,
        sha256,
    })
}

fn sha256(path: &Path, binary_name: &str) -> Result<String, String> {
    let checksum = Command::new("shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()
        .map_err(|error| format!("could not calculate {binary_name} SHA-256: {error}"))?;
    if !checksum.status.success() {
        return Err(format!(
            "{binary_name} checksum failed with {}: {}",
            checksum.status,
            summarize_stderr(&checksum.stderr)
        ));
    }
    let checksum_output = String::from_utf8(checksum.stdout)
        .map_err(|error| format!("checksum output was not UTF-8: {error}"))?;
    let sha256 = checksum_output
        .split_whitespace()
        .next()
        .filter(|value| {
            value.len() == 64 && value.chars().all(|character| character.is_ascii_hexdigit())
        })
        .ok_or_else(|| format!("unexpected shasum output: {checksum_output:?}"))?
        .to_ascii_lowercase();
    Ok(sha256)
}

fn summarize_stderr(stderr: &[u8]) -> String {
    let rendered = String::from_utf8_lossy(stderr);
    let mut summary = rendered.trim().chars().take(2_000).collect::<String>();
    if rendered.trim().chars().count() > 2_000 {
        summary.push_str("...");
    }
    if summary.is_empty() {
        "<no stderr>".to_owned()
    } else {
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amplification_repeats_the_same_query_exactly() {
        let batch =
            sql_batch("CREATE TABLE t (n Int64);\n", "SELECT n FROM t;", 3).expect("valid batch");
        assert_eq!(batch.matches("CREATE TABLE").count(), 1);
        assert_eq!(batch.matches("SELECT n FROM t;").count(), 3);
    }

    #[test]
    fn amplification_must_be_positive() {
        assert!(sql_batch("", "SELECT 1;", 0).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_is_bound_to_copied_bytes_not_the_source_path() {
        use std::os::unix::fs::PermissionsExt as _;

        let source_directory = create_snapshot_directory("source-test").expect("source directory");
        let source = source_directory.join("rusthouse");
        fs::write(&source, "#!/bin/sh\nexit 0\n").expect("source binary");
        fs::set_permissions(&source, fs::Permissions::from_mode(0o700)).expect("permissions");

        let snapshot = RustHouseSnapshot::create(&source, "candidate").expect("snapshot");
        let identity = validate_rusthouse(snapshot.path()).expect("identity");
        assert_eq!(
            fs::metadata(&snapshot.directory)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o500
        );
        assert_eq!(
            fs::metadata(snapshot.path()).unwrap().permissions().mode() & 0o777,
            0o500
        );
        fs::write(&source, "#!/bin/sh\nexit 9\n").expect("replace source bytes");

        validate_rusthouse(snapshot.path()).expect("snapshot remains executable");
        snapshot
            .verify_unchanged(&identity, "candidate")
            .expect("snapshot hash remains bound");

        fs::remove_file(source).expect("remove source");
        fs::remove_dir(source_directory).expect("remove source directory");
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_tampering_is_rejected() {
        use std::os::unix::fs::PermissionsExt as _;

        let source_directory =
            create_snapshot_directory("tamper-source").expect("source directory");
        let source = source_directory.join("rusthouse");
        fs::write(&source, "#!/bin/sh\nexit 0\n").expect("source binary");
        fs::set_permissions(&source, fs::Permissions::from_mode(0o700)).expect("permissions");

        let snapshot = RustHouseSnapshot::create(&source, "baseline").expect("snapshot");
        let identity = validate_rusthouse(snapshot.path()).expect("identity");
        make_snapshot_writable(snapshot.path());
        fs::write(snapshot.path(), "#!/bin/sh\nexit 7\n").expect("tamper snapshot");
        make_snapshot_read_only(snapshot.path()).expect("restore snapshot permissions");

        let error = snapshot
            .verify_unchanged(&identity, "baseline")
            .expect_err("changed snapshot must fail");
        assert!(error.contains("changed during the benchmark"));

        fs::remove_file(source).expect("remove source");
        fs::remove_dir(source_directory).expect("remove source directory");
    }
}
