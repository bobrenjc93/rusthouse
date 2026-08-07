use sha2::{Digest as _, Sha256};
use std::env;
use std::io::{Read, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
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
pub struct BenchmarkIdentity {
    pub clickhouse_version_output: String,
    pub clickhouse_sha256: String,
    pub rusthouse_sha256: String,
    pub rusthouse_source_commit: String,
    pub rusthouse_build_profile: String,
    pub rustflags: String,
    pub harness_sha256: String,
    pub os: String,
    pub cpu: String,
    pub rust_toolchain: String,
}

struct ClickHouseIdentity {
    version_output: String,
    sha256: String,
}

#[derive(Debug, Clone, Copy)]
pub enum Engine {
    RustHouse,
    ClickHouse,
}

#[derive(Debug)]
pub struct TimedOutput {
    pub stdout: String,
    pub digest: OutputDigest,
}

#[derive(Debug)]
pub struct TimedBatch {
    pub elapsed: Duration,
    pub query_repetitions: usize,
    pub output_digest: OutputDigest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputDigest {
    pub bytes: u64,
    pub sha256: String,
}

impl OutputDigest {
    pub fn repeated(bytes: &[u8], repetitions: usize) -> Result<Self, String> {
        if repetitions == 0 {
            return Err("output digest repetition count must be positive".to_owned());
        }
        let byte_count = u64::try_from(bytes.len())
            .ok()
            .and_then(|length| length.checked_mul(repetitions as u64))
            .ok_or_else(|| "repeated output byte count overflowed".to_owned())?;
        let mut hasher = Sha256::new();
        for _ in 0..repetitions {
            hasher.update(bytes);
        }
        Ok(Self {
            bytes: byte_count,
            sha256: format!("{:x}", hasher.finalize()),
        })
    }
}

impl EnginePaths {
    pub fn validate(&self) -> Result<BenchmarkIdentity, String> {
        validate_rusthouse(&self.rusthouse)?;
        let clickhouse = validate_clickhouse(&self.clickhouse)?;
        let executable = env::current_exe()
            .map_err(|error| format!("could not locate benchmark executable: {error}"))?;
        Ok(BenchmarkIdentity {
            clickhouse_version_output: clickhouse.version_output,
            clickhouse_sha256: clickhouse.sha256,
            rusthouse_sha256: sha256_file(&self.rusthouse, "RustHouse")?,
            rusthouse_source_commit: command_text(
                "git",
                &["rev-parse", "HEAD"],
                "RustHouse source commit",
            )?,
            rusthouse_build_profile: infer_build_profile(&self.rusthouse)?,
            rustflags: env::var("RUSTFLAGS").unwrap_or_else(|_| "<unset>".to_owned()),
            harness_sha256: sha256_file(&executable, "benchmark harness")?,
            os: command_text("uname", &["-a"], "operating system identity")?,
            cpu: command_text(
                "sysctl",
                &["-n", "machdep.cpu.brand_string"],
                "CPU identity",
            )
            .unwrap_or_else(|_| env::consts::ARCH.to_owned()),
            rust_toolchain: command_text("rustc", &["--version", "--verbose"], "Rust toolchain")?,
        })
    }

    pub fn execute_correctness(
        &self,
        engine: Engine,
        setup_sql: &str,
        query_sql: &str,
    ) -> Result<TimedOutput, String> {
        let batch = sql_batch(setup_sql, query_sql, 1)?;
        let (_, stdout, digest) = self.execute_batch(engine, &batch, true)?;
        Ok(TimedOutput {
            stdout: stdout.expect("captured execution returns stdout"),
            digest,
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
        let (elapsed, stdout, output_digest) = self.execute_batch(engine, &batch, false)?;
        debug_assert!(stdout.is_none());
        Ok(TimedBatch {
            elapsed,
            query_repetitions,
            output_digest,
        })
    }

    fn execute_batch(
        &self,
        engine: Engine,
        batch: &str,
        capture_stdout: bool,
    ) -> Result<(Duration, Option<String>, OutputDigest), String> {
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
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let started = Instant::now();
        let mut child = command.spawn().map_err(|error| {
            format!(
                "could not start {} at '{}': {error}",
                engine.name(),
                engine.path(self).display()
            )
        })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| format!("{} stdout was not piped", engine.name()))?;
        let output_reader = thread::spawn(move || digest_output(stdout, capture_stdout));
        {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| format!("{} stdin was not piped", engine.name()))?;
            stdin
                .write_all(batch.as_bytes())
                .map_err(|error| format!("could not write SQL to {}: {error}", engine.name()))?;
        }
        let output = child
            .wait_with_output()
            .map_err(|error| format!("could not wait for {}: {error}", engine.name()))?;
        let elapsed = started.elapsed();
        let captured = output_reader
            .join()
            .map_err(|_| format!("{} output digest worker panicked", engine.name()))??;

        if !output.status.success() {
            return Err(format!(
                "{} exited with {}: {}",
                engine.name(),
                output.status,
                summarize_stderr(&output.stderr)
            ));
        }
        let stdout = captured
            .bytes
            .map(|bytes| {
                String::from_utf8(bytes)
                    .map_err(|error| format!("{} emitted non-UTF-8 output: {error}", engine.name()))
            })
            .transpose()?;
        Ok((elapsed, stdout, captured.digest))
    }
}

struct CapturedOutput {
    digest: OutputDigest,
    bytes: Option<Vec<u8>>,
}

fn digest_output(mut reader: impl Read, capture: bool) -> Result<CapturedOutput, String> {
    let mut hasher = Sha256::new();
    let mut byte_count = 0_u64;
    let mut captured = capture.then(Vec::new);
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("could not read engine output: {error}"))?;
        if read == 0 {
            break;
        }
        byte_count = byte_count
            .checked_add(read as u64)
            .ok_or_else(|| "engine output byte count overflowed".to_owned())?;
        hasher.update(&buffer[..read]);
        if let Some(bytes) = &mut captured {
            bytes.extend_from_slice(&buffer[..read]);
        }
    }
    Ok(CapturedOutput {
        digest: OutputDigest {
            bytes: byte_count,
            sha256: format!("{:x}", hasher.finalize()),
        },
        bytes: captured,
    })
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

fn infer_build_profile(path: &Path) -> Result<String, String> {
    for component in path.components().rev() {
        let component = component.as_os_str().to_string_lossy();
        if component == "release" || component == "debug" {
            return Ok(component.into_owned());
        }
    }
    Err(format!(
        "could not infer RustHouse build profile from '{}'",
        path.display()
    ))
}

fn command_text(program: &str, arguments: &[&str], label: &str) -> Result<String, String> {
    let output = Command::new(program)
        .args(arguments)
        .output()
        .map_err(|error| format!("could not collect {label}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{label} command failed with {}: {}",
            output.status,
            summarize_stderr(&output.stderr)
        ));
    }
    let value = String::from_utf8(output.stdout)
        .map_err(|error| format!("{label} was not UTF-8: {error}"))?
        .trim()
        .to_owned();
    if value.is_empty() {
        return Err(format!("{label} was empty"));
    }
    Ok(value)
}

fn sha256_file(path: &Path, label: &str) -> Result<String, String> {
    let output = Command::new("shasum")
        .args(["-a", "256"])
        .arg(path)
        .output()
        .map_err(|error| format!("could not calculate {label} SHA-256: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{label} checksum failed with {}: {}",
            output.status,
            summarize_stderr(&output.stderr)
        ));
    }
    let checksum_output = String::from_utf8(output.stdout)
        .map_err(|error| format!("{label} checksum output was not UTF-8: {error}"))?;
    checksum_output
        .split_whitespace()
        .next()
        .filter(|value| {
            value.len() == 64 && value.chars().all(|character| character.is_ascii_hexdigit())
        })
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| format!("unexpected {label} shasum output: {checksum_output:?}"))
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
}
