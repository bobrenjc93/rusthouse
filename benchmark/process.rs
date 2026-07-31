use std::env;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rusthouse::build_info::{ATTESTATION_VERSION, BuildInfo};

use crate::sha256;

pub const CLICKHOUSE_VERSION: &str = "26.7.1";
pub const CLICKHOUSE_SHA256: &str =
    "6611c5aadcfac188031fa0fdf2676ec311771f96654a62b918b146b60dd11075";
pub const CLICKHOUSE_ARTIFACT_URL: &str = "https://github.com/ClickHouse/ClickHouse/releases/download/v26.7.1.1315-stable/clickhouse-macos-aarch64";
pub const CLICKHOUSE_ARTIFACT_PLATFORM: &str = "macos-aarch64";
const CLICKHOUSE_TARGET: &str = "aarch64-apple-darwin";

#[derive(Debug, Clone)]
pub struct EnginePaths {
    pub rusthouse: PathBuf,
    pub clickhouse: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClickHouseIdentity {
    pub version_output: String,
    pub sha256: String,
    pub artifact_url: &'static str,
    pub artifact_platform: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustHouseIdentity {
    pub sha256: String,
    pub source_commit: String,
    pub source_dirty: bool,
    pub rustc_version: String,
    pub target: String,
    pub profile: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostIdentity {
    pub platform: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunIdentity {
    pub rusthouse: RustHouseIdentity,
    pub clickhouse: ClickHouseIdentity,
    pub host: HostIdentity,
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
    pub fn validate(&self, expected: BuildInfo) -> Result<RunIdentity, String> {
        let rusthouse = validate_rusthouse(&self.rusthouse)?;
        validate_rusthouse_build(&rusthouse, expected)?;
        let clickhouse = validate_clickhouse(&self.clickhouse)?;
        let host = validate_host(&rusthouse)?;
        Ok(RunIdentity {
            rusthouse,
            clickhouse,
            host,
        })
    }

    pub fn revalidate(&self, expected: BuildInfo, original: &RunIdentity) -> Result<(), String> {
        let current = self.validate(expected)?;
        ensure_identity_unchanged(original, &current)
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

fn ensure_identity_unchanged(original: &RunIdentity, current: &RunIdentity) -> Result<(), String> {
    if current != original {
        return Err(
            "benchmark provenance changed while the suite was running; no report was retained"
                .to_owned(),
        );
    }
    Ok(())
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

fn validate_rusthouse(path: &Path) -> Result<RustHouseIdentity, String> {
    let sha256 = sha256::file_digest_hex(path)?;
    let output = Command::new(path)
        .arg("--benchmark-attestation")
        .output()
        .map_err(|error| {
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
    let attestation = String::from_utf8(output.stdout)
        .map_err(|error| format!("RustHouse attestation was not UTF-8: {error}"))?;
    parse_rusthouse_attestation(&attestation, sha256)
}

fn parse_rusthouse_attestation(
    attestation: &str,
    sha256: String,
) -> Result<RustHouseIdentity, String> {
    let mut lines = attestation.lines();
    if lines.next() != Some(ATTESTATION_VERSION) {
        return Err(
            "RustHouse build attestation is missing or has an unsupported version".to_owned(),
        );
    }

    let mut source_commit = None;
    let mut source_dirty = None;
    let mut rustc_version = None;
    let mut target = None;
    let mut profile = None;
    for line in lines {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("malformed RustHouse attestation line {line:?}"))?;
        let destination = match key {
            "source_commit" => &mut source_commit,
            "source_dirty" => &mut source_dirty,
            "rustc_version" => &mut rustc_version,
            "target" => &mut target,
            "profile" => &mut profile,
            _ => return Err(format!("unknown RustHouse attestation field {key:?}")),
        };
        if destination.replace(value.to_owned()).is_some() {
            return Err(format!("duplicate RustHouse attestation field {key:?}"));
        }
    }

    let source_commit = required_attestation_field(source_commit, "source_commit")?;
    if !matches!(source_commit.len(), 40 | 64)
        || !source_commit
            .chars()
            .all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase())
    {
        return Err("RustHouse attestation contains an invalid source_commit".to_owned());
    }
    let source_dirty = match required_attestation_field(source_dirty, "source_dirty")?.as_str() {
        "true" => true,
        "false" => false,
        _ => return Err("RustHouse attestation contains an invalid source_dirty".to_owned()),
    };
    let rustc_version = required_attestation_field(rustc_version, "rustc_version")?;
    if !rustc_version.starts_with("rustc ") {
        return Err("RustHouse attestation contains an invalid rustc_version".to_owned());
    }
    let target = required_attestation_field(target, "target")?;
    let profile = required_attestation_field(profile, "profile")?;
    if target.chars().any(char::is_whitespace) || profile.chars().any(char::is_whitespace) {
        return Err("RustHouse target and profile must not contain whitespace".to_owned());
    }

    Ok(RustHouseIdentity {
        sha256,
        source_commit,
        source_dirty,
        rustc_version,
        target,
        profile,
    })
}

fn required_attestation_field(value: Option<String>, name: &str) -> Result<String, String> {
    value
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("RustHouse attestation is missing required field {name:?}"))
}

fn validate_rusthouse_build(actual: &RustHouseIdentity, expected: BuildInfo) -> Result<(), String> {
    if actual.source_commit != expected.source_commit
        || actual.source_dirty != expected.source_dirty
        || actual.rustc_version != expected.rustc_version
        || actual.target != expected.target
        || actual.profile != expected.profile
    {
        return Err(format!(
            "RustHouse build attestation is inconsistent with the benchmark binary: RustHouse commit={} dirty={} rustc={:?} target={} profile={}; benchmark commit={} dirty={} rustc={:?} target={} profile={}",
            actual.source_commit,
            actual.source_dirty,
            actual.rustc_version,
            actual.target,
            actual.profile,
            expected.source_commit,
            expected.source_dirty,
            expected.rustc_version,
            expected.target,
            expected.profile,
        ));
    }
    Ok(())
}

fn validate_host(rusthouse: &RustHouseIdentity) -> Result<HostIdentity, String> {
    let platform = format!("{}-{}", env::consts::OS, env::consts::ARCH);
    if platform != CLICKHOUSE_ARTIFACT_PLATFORM {
        return Err(format!(
            "host platform {platform:?} is inconsistent with pinned ClickHouse artifact platform {CLICKHOUSE_ARTIFACT_PLATFORM:?}"
        ));
    }
    if rusthouse.target != CLICKHOUSE_TARGET {
        return Err(format!(
            "RustHouse target {:?} is inconsistent with pinned ClickHouse target {CLICKHOUSE_TARGET:?}",
            rusthouse.target
        ));
    }

    let output = Command::new("uname")
        .args(["-s", "-r", "-m"])
        .output()
        .map_err(|error| format!("could not determine host platform with uname: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "host platform check failed with {}: {}",
            output.status,
            summarize_stderr(&output.stderr)
        ));
    }
    let description = String::from_utf8(output.stdout)
        .map_err(|error| format!("host platform output was not UTF-8: {error}"))?
        .trim()
        .to_owned();
    if description.is_empty() {
        return Err("host platform output was empty".to_owned());
    }
    let host_fields = description.split_whitespace().collect::<Vec<_>>();
    if host_fields.first() != Some(&"Darwin") || host_fields.last() != Some(&"arm64") {
        return Err(format!(
            "host description {description:?} is inconsistent with platform {CLICKHOUSE_ARTIFACT_PLATFORM:?}"
        ));
    }
    Ok(HostIdentity {
        platform,
        description,
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

    let sha256 = sha256::file_digest_hex(path)?;

    if sha256 != CLICKHOUSE_SHA256 {
        return Err(format!(
            "ClickHouse checksum mismatch: expected {CLICKHOUSE_SHA256}, got {sha256}"
        ));
    }

    Ok(ClickHouseIdentity {
        version_output,
        sha256,
        artifact_url: CLICKHOUSE_ARTIFACT_URL,
        artifact_platform: CLICKHOUSE_ARTIFACT_PLATFORM,
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

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    fn build_info() -> BuildInfo {
        BuildInfo {
            source_commit: COMMIT,
            source_dirty: false,
            rustc_version: "rustc 1.88.0 (test)",
            target: CLICKHOUSE_TARGET,
            profile: "release",
        }
    }

    fn attestation() -> String {
        format!(
            "{ATTESTATION_VERSION}\nsource_commit={COMMIT}\nsource_dirty=false\nrustc_version=rustc 1.88.0 (test)\ntarget={CLICKHOUSE_TARGET}\nprofile=release\n"
        )
    }

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

    #[test]
    fn missing_attestation_field_is_rejected() {
        let incomplete = attestation().replace("profile=release\n", "");
        let error = parse_rusthouse_attestation(&incomplete, "a".repeat(64))
            .expect_err("missing profile must fail");
        assert!(error.contains("profile"));
    }

    #[test]
    fn tampered_attestation_is_rejected_as_inconsistent() {
        let tampered = attestation().replace("source_dirty=false", "source_dirty=true");
        let identity = parse_rusthouse_attestation(&tampered, "a".repeat(64))
            .expect("well-formed attestation");
        let error = validate_rusthouse_build(&identity, build_info())
            .expect_err("tampered metadata must fail");
        assert!(error.contains("inconsistent"));
    }

    #[test]
    fn binary_tampering_during_a_run_is_rejected() {
        let rusthouse =
            parse_rusthouse_attestation(&attestation(), "a".repeat(64)).expect("valid attestation");
        let original = RunIdentity {
            rusthouse,
            clickhouse: ClickHouseIdentity {
                version_output: "ClickHouse 26.7.1".to_owned(),
                sha256: CLICKHOUSE_SHA256.to_owned(),
                artifact_url: CLICKHOUSE_ARTIFACT_URL,
                artifact_platform: CLICKHOUSE_ARTIFACT_PLATFORM,
            },
            host: HostIdentity {
                platform: CLICKHOUSE_ARTIFACT_PLATFORM.to_owned(),
                description: "Darwin test arm64".to_owned(),
            },
        };
        let mut tampered = original.clone();
        tampered.rusthouse.sha256 = "b".repeat(64);

        let error = ensure_identity_unchanged(&original, &tampered)
            .expect_err("changed binary digest must fail");
        assert!(error.contains("provenance changed"));
    }
}
