use std::io::{Read, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::normalize::{ColumnType, ResultOracle, ValidationSummary, validate_repeated_outputs};

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
    pub verified_results: usize,
    pub canonical_digest: String,
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
        let (_, stdout) = self.execute_batch(engine, &batch)?;
        Ok(TimedOutput { stdout })
    }

    pub fn execute_timed(
        &self,
        engine: Engine,
        setup_sql: &str,
        query_sql: &str,
        query_repetitions: usize,
        columns: &[(&str, ColumnType)],
        oracle: &ResultOracle,
    ) -> Result<TimedBatch, String> {
        let batch = sql_batch(setup_sql, query_sql, query_repetitions)?;
        let (elapsed, validation) =
            self.execute_validated_batch(engine, &batch, query_repetitions, columns, oracle)?;
        Ok(TimedBatch {
            elapsed,
            query_repetitions,
            verified_results: validation.verified_results,
            canonical_digest: validation.canonical_digest,
        })
    }

    fn execute_validated_batch(
        &self,
        engine: Engine,
        batch: &str,
        query_repetitions: usize,
        columns: &[(&str, ColumnType)],
        oracle: &ResultOracle,
    ) -> Result<(Duration, ValidationSummary), String> {
        let mut command = self.command(engine);
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
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| format!("{} stdin was not piped", engine.name()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| format!("{} stdout was not piped", engine.name()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| format!("{} stderr was not piped", engine.name()))?;

        let (status, elapsed, write_result, validation, stderr) = thread::scope(|scope| {
            let validation = scope.spawn(|| {
                validate_repeated_outputs(stdout, oracle, columns, engine.name(), query_repetitions)
            });
            let stderr = scope.spawn(|| read_bounded(stderr));

            let write_result = stdin
                .write_all(batch.as_bytes())
                .map_err(|error| format!("could not write SQL to {}: {error}", engine.name()));
            drop(stdin);
            let status = child
                .wait()
                .map_err(|error| format!("could not wait for {}: {error}", engine.name()));
            let elapsed = started.elapsed();
            let validation = validation
                .join()
                .map_err(|_| format!("{} stdout validator panicked", engine.name()));
            let stderr = stderr
                .join()
                .map_err(|_| format!("{} stderr reader panicked", engine.name()));
            (status, elapsed, write_result, validation, stderr)
        });

        let status = status?;
        let validation = validation?;
        let stderr = stderr??;
        if !status.success() {
            let validation_context = validation
                .as_ref()
                .err()
                .map(|error| format!("; stdout validation also failed: {error}"))
                .unwrap_or_default();
            return Err(format!(
                "{} exited with {}: {}{}",
                engine.name(),
                status,
                summarize_stderr(&stderr),
                validation_context
            ));
        }
        write_result?;
        let validation = validation?;
        Ok((elapsed, validation))
    }

    fn execute_batch(&self, engine: Engine, batch: &str) -> Result<(Duration, String), String> {
        let mut command = self.command(engine);
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
        let stdout = String::from_utf8(output.stdout)
            .map_err(|error| format!("{} emitted non-UTF-8 output: {error}", engine.name()))?;
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

fn read_bounded(mut reader: impl Read) -> Result<Vec<u8>, String> {
    const MAX_CAPTURED_STDERR_BYTES: usize = 64 * 1024;

    let mut captured = Vec::new();
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| format!("could not read child stderr: {error}"))?;
        if count == 0 {
            break;
        }
        let remaining = MAX_CAPTURED_STDERR_BYTES.saturating_sub(captured.len());
        captured.extend_from_slice(&buffer[..count.min(remaining)]);
    }
    Ok(captured)
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
}
