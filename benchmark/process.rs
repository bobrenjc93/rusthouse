#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::io::Read as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::{mem::MaybeUninit, os::unix::process::ExitStatusExt as _};

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

#[derive(Debug)]
pub struct ResourceMeasurement {
    pub elapsed: Duration,
    pub peak_rss_bytes: u64,
}

pub fn ensure_resource_measurement_supported() -> Result<(), String> {
    if cfg!(any(target_os = "linux", target_os = "macos")) {
        Ok(())
    } else {
        Err(format!(
            "resource measurement is unsupported on {}; peak RSS collection requires macOS or Linux wait4",
            std::env::consts::OS
        ))
    }
}

pub fn peak_rss_normalization() -> Result<&'static str, String> {
    ensure_resource_measurement_supported()?;
    if cfg!(target_os = "macos") {
        Ok("macOS wait4 ru_maxrss bytes")
    } else {
        Ok("Linux wait4 ru_maxrss KiB multiplied by 1024")
    }
}

impl EnginePaths {
    pub fn validate(&self) -> Result<ClickHouseIdentity, String> {
        validate_rusthouse(&self.rusthouse)?;
        validate_clickhouse(&self.clickhouse)
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

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn execute_ingestion(
        &self,
        engine: Engine,
        setup_sql: &str,
    ) -> Result<ResourceMeasurement, String> {
        if setup_sql.is_empty() {
            return Err("resource measurement setup SQL must not be empty".to_owned());
        }
        let mut command = self.command(engine);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let started = Instant::now();
        let mut child = command.spawn().map_err(|error| {
            format!(
                "could not start {} at '{}': {error}",
                engine.name(),
                engine.path(self).display()
            )
        })?;
        let input_result = child
            .stdin
            .take()
            .expect("configured resource stdin is piped")
            .write_all(setup_sql.as_bytes());

        let mut stderr = Vec::new();
        let stderr_result = child
            .stderr
            .take()
            .expect("configured resource stderr is piped")
            .read_to_end(&mut stderr);
        let (status, usage) = wait_with_rusage(&child)?;
        let elapsed = started.elapsed();

        if !status.success() {
            return Err(format!(
                "{} resource process exited with {}: {}",
                engine.name(),
                status,
                summarize_stderr(&stderr)
            ));
        }
        input_result
            .map_err(|error| format!("could not write setup SQL to {}: {error}", engine.name()))?;
        stderr_result
            .map_err(|error| format!("could not read {} stderr: {error}", engine.name()))?;
        let peak_rss_bytes = normalize_peak_rss_bytes(usage.ru_maxrss)?;
        if elapsed.is_zero() {
            return Err(format!(
                "{} ingestion wall time was zero; measurement is incomplete",
                engine.name()
            ));
        }
        Ok(ResourceMeasurement {
            elapsed,
            peak_rss_bytes,
        })
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub fn execute_ingestion(
        &self,
        _engine: Engine,
        _setup_sql: &str,
    ) -> Result<ResourceMeasurement, String> {
        ensure_resource_measurement_supported()?;
        unreachable!("unsupported resource measurement was accepted")
    }

    fn execute_batch(
        &self,
        engine: Engine,
        batch: &str,
        capture_stdout: bool,
    ) -> Result<(Duration, Option<String>), String> {
        let mut command = self.command(engine);
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

    fn command(&self, engine: Engine) -> Command {
        match engine {
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
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn wait_with_rusage(
    child: &std::process::Child,
) -> Result<(std::process::ExitStatus, libc::rusage), String> {
    let pid = libc::pid_t::try_from(child.id())
        .map_err(|_| format!("child PID {} does not fit pid_t", child.id()))?;
    let mut status = 0;
    let mut usage = MaybeUninit::<libc::rusage>::uninit();
    loop {
        // Stderr has already been drained, and wait4 atomically returns the
        // child's exit status and per-process high-water RSS.
        let waited = unsafe { libc::wait4(pid, &mut status, 0, usage.as_mut_ptr()) };
        if waited == pid {
            // wait4 initialized rusage when it returned this child's PID.
            let usage = unsafe { usage.assume_init() };
            return Ok((std::process::ExitStatus::from_raw(status), usage));
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return Err(format!("could not collect child resource usage: {error}"));
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn normalize_peak_rss_bytes(native_peak_rss: libc::c_long) -> Result<u64, String> {
    let native_peak_rss = u64::try_from(native_peak_rss).map_err(|_| {
        format!("peak RSS was negative ({native_peak_rss}); measurement is incomplete")
    })?;
    if native_peak_rss == 0 {
        return Err("peak RSS was zero; measurement is incomplete".to_owned());
    }
    if cfg!(target_os = "linux") {
        native_peak_rss
            .checked_mul(1_024)
            .ok_or_else(|| "peak RSS overflowed while normalizing KiB to bytes".to_owned())
    } else {
        Ok(native_peak_rss)
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

fn validate_rusthouse(path: &Path) -> Result<(), String> {
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
    Ok(())
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

    let checksum = Command::new("shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()
        .map_err(|error| format!("could not calculate ClickHouse SHA-256: {error}"))?;
    if !checksum.status.success() {
        return Err(format!(
            "ClickHouse checksum failed with {}: {}",
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

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn peak_rss_is_positive_and_normalized_to_bytes() {
        assert!(normalize_peak_rss_bytes(0).is_err());
        let normalized = normalize_peak_rss_bytes(1_024).expect("positive RSS");
        if cfg!(target_os = "linux") {
            assert_eq!(normalized, 1_048_576);
        } else {
            assert_eq!(normalized, 1_024);
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn ingestion_measurement_collects_real_child_rss() {
        use std::os::unix::fs::PermissionsExt as _;

        let script = std::env::temp_dir().join(format!(
            "rusthouse-resource-measurement-{}",
            std::process::id()
        ));
        std::fs::write(&script, "#!/bin/sh\nwhile IFS= read -r line; do :; done\n")
            .expect("write test command");
        let mut permissions = std::fs::metadata(&script).expect("metadata").permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&script, permissions).expect("make test command executable");

        let paths = EnginePaths {
            rusthouse: script.clone(),
            clickhouse: script.clone(),
        };
        let measurement = paths
            .execute_ingestion(Engine::RustHouse, "CREATE TABLE t (n Int64);\n")
            .expect("resource measurement");
        let _ = std::fs::remove_file(script);

        assert!(!measurement.elapsed.is_zero());
        assert!(measurement.peak_rss_bytes > 0);
    }
}
