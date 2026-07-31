use std::env;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitCode, Stdio};
use std::time::{Duration, Instant, SystemTime};

use rusthouse::build_info::{ATTESTATION_VERSION, BuildInfo};

use crate::sha256;

pub const CLICKHOUSE_VERSION: &str = "26.7.1";
pub const CLICKHOUSE_SHA256: &str =
    "6611c5aadcfac188031fa0fdf2676ec311771f96654a62b918b146b60dd11075";
pub const CLICKHOUSE_ARTIFACT_URL: &str = "https://github.com/ClickHouse/ClickHouse/releases/download/v26.7.1.1315-stable/clickhouse-macos-aarch64";
pub const CLICKHOUSE_ARTIFACT_PLATFORM: &str = "macos-aarch64";
const CLICKHOUSE_TARGET: &str = "aarch64-apple-darwin";
const STAGING_DIRECTORY_PREFIX: &str = "rusthouse-benchmark-pinned-";
const STAGING_LIVENESS_FILE: &str = ".active.lock";
const STALE_STAGING_AGE: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone)]
pub struct EnginePaths {
    pub rusthouse: PathBuf,
    pub clickhouse: PathBuf,
}

#[derive(Debug)]
pub struct PinnedExecutables {
    directory: PathBuf,
    cleanup_guard: Option<CleanupGuard>,
    liveness_lock: Option<fs::File>,
}

#[derive(Debug)]
struct CleanupGuard {
    child: Child,
    stdin: Option<ChildStdin>,
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
    pub build_configuration_sha256: String,
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
    pub fn pin_and_validate(
        &self,
        expected: BuildInfo,
        expected_rusthouse_sha256: &str,
    ) -> Result<(Self, RunIdentity, PinnedExecutables), String> {
        let pinned = PinnedExecutables::create(self)?;
        let paths = pinned.paths();
        let identity = paths.validate(expected, expected_rusthouse_sha256)?;
        Ok((paths, identity, pinned))
    }

    pub fn validate(
        &self,
        expected: BuildInfo,
        expected_rusthouse_sha256: &str,
    ) -> Result<RunIdentity, String> {
        let rusthouse_sha256 =
            validate_executable_sha256(&self.rusthouse, "RustHouse", expected_rusthouse_sha256)?;
        let clickhouse_sha256 =
            validate_executable_sha256(&self.clickhouse, "ClickHouse", CLICKHOUSE_SHA256)?;
        let rusthouse = validate_rusthouse(&self.rusthouse, rusthouse_sha256)?;
        validate_rusthouse_build(&rusthouse, expected)?;
        let clickhouse = validate_clickhouse(&self.clickhouse, clickhouse_sha256)?;
        let host = validate_host(&rusthouse)?;
        Ok(RunIdentity {
            rusthouse,
            clickhouse,
            host,
        })
    }

    pub fn revalidate(
        &self,
        expected: BuildInfo,
        expected_rusthouse_sha256: &str,
        original: &RunIdentity,
    ) -> Result<(), String> {
        let current = self.validate(expected, expected_rusthouse_sha256)?;
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

impl PinnedExecutables {
    fn create(sources: &EnginePaths) -> Result<Self, String> {
        let directory = create_private_staging_directory()?;
        let liveness_lock = match create_staging_liveness_lock(&directory) {
            Ok(lock) => lock,
            Err(error) => {
                let _ = cleanup_staging_directory(&directory);
                return Err(error);
            }
        };
        let cleanup_guard = match start_cleanup_guard(&directory) {
            Ok(guard) => guard,
            Err(error) => {
                drop(liveness_lock);
                let _ = cleanup_staging_directory(&directory);
                return Err(error);
            }
        };
        let pinned = Self {
            directory,
            cleanup_guard,
            liveness_lock: Some(liveness_lock),
        };
        copy_executable(&sources.rusthouse, &pinned.rusthouse_path())?;
        copy_executable(&sources.clickhouse, &pinned.clickhouse_path())?;
        make_staging_directory_read_only(&pinned.directory)?;
        Ok(pinned)
    }

    fn paths(&self) -> EnginePaths {
        EnginePaths {
            rusthouse: self.rusthouse_path(),
            clickhouse: self.clickhouse_path(),
        }
    }

    fn rusthouse_path(&self) -> PathBuf {
        self.directory
            .join(format!("rusthouse-pinned{}", env::consts::EXE_SUFFIX))
    }

    fn clickhouse_path(&self) -> PathBuf {
        self.directory
            .join(format!("clickhouse-pinned{}", env::consts::EXE_SUFFIX))
    }
}

impl Drop for PinnedExecutables {
    fn drop(&mut self) {
        drop(self.liveness_lock.take());
        if let Some(mut guard) = self.cleanup_guard.take() {
            guard.finish();
        }
        let _ = cleanup_staging_directory(&self.directory);
    }
}

fn create_staging_liveness_lock(directory: &Path) -> Result<fs::File, String> {
    let path = directory.join(STAGING_LIVENESS_FILE);
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| {
            format!(
                "could not create staging liveness marker '{}': {error}",
                path.display()
            )
        })?;
    file.lock().map_err(|error| {
        format!(
            "could not lock staging liveness marker '{}': {error}",
            path.display()
        )
    })?;
    Ok(file)
}

fn create_private_staging_directory() -> Result<PathBuf, String> {
    let parent = env::temp_dir();
    scavenge_stale_staging_directories(&parent, STALE_STAGING_AGE)?;
    for attempt in 0..100_u32 {
        let directory = parent.join(format!(
            "{STAGING_DIRECTORY_PREFIX}{}-{attempt}",
            std::process::id()
        ));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        match builder.create(&directory) {
            Ok(()) => return Ok(directory),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "could not create private executable staging directory in '{}': {error}",
                    parent.display()
                ));
            }
        }
    }
    Err(format!(
        "could not reserve a private executable staging directory in '{}'",
        parent.display()
    ))
}

#[cfg(not(test))]
fn start_cleanup_guard(directory: &Path) -> Result<Option<CleanupGuard>, String> {
    let executable = env::current_exe()
        .map_err(|error| format!("could not locate benchmark cleanup guardian: {error}"))?;
    let mut command = Command::new(executable);
    command
        .arg("--internal-staging-cleanup-guard")
        .arg(directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|error| {
        format!(
            "could not start staging cleanup guardian for '{}': {error}",
            directory.display()
        )
    })?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "staging cleanup guardian stdin was not piped".to_owned())?;
    Ok(Some(CleanupGuard {
        child,
        stdin: Some(stdin),
    }))
}

#[cfg(test)]
fn start_cleanup_guard(directory: &Path) -> Result<Option<CleanupGuard>, String> {
    if directory.as_os_str().is_empty() {
        return Err("staging directory is empty".to_owned());
    }
    Ok(None)
}

impl CleanupGuard {
    fn finish(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.wait();
    }
}

pub fn run_staging_cleanup_guard_if_requested() -> Option<ExitCode> {
    let mut arguments = env::args_os().skip(1);
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new("--internal-staging-cleanup-guard"))
    {
        return None;
    }
    let result = match (arguments.next(), arguments.next()) {
        (Some(directory), None) => {
            cleanup_staging_directory_after_eof(std::io::stdin().lock(), Path::new(&directory))
        }
        _ => Err("cleanup guardian requires exactly one staging directory".to_owned()),
    };
    Some(match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("staging cleanup guardian failed: {error}");
            ExitCode::FAILURE
        }
    })
}

fn cleanup_staging_directory_after_eof(
    mut parent_pipe: impl std::io::Read,
    directory: &Path,
) -> Result<(), String> {
    let mut buffer = [0_u8; 64];
    loop {
        match parent_pipe.read(&mut buffer) {
            Ok(0) => break,
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("could not monitor benchmark parent: {error}")),
        }
    }
    validate_staging_directory_path(directory)?;
    cleanup_staging_directory(directory)
}

fn validate_staging_directory_path(directory: &Path) -> Result<(), String> {
    let file_name = directory
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| name.starts_with(STAGING_DIRECTORY_PREFIX))
        .ok_or_else(|| {
            format!(
                "refusing to clean non-staging path '{}'",
                directory.display()
            )
        })?;
    if file_name.len() == STAGING_DIRECTORY_PREFIX.len() {
        return Err(format!(
            "refusing to clean malformed staging path '{}'",
            directory.display()
        ));
    }
    let parent = directory
        .parent()
        .ok_or_else(|| format!("staging path '{}' has no parent", directory.display()))?;
    let expected_parent = fs::canonicalize(env::temp_dir())
        .map_err(|error| format!("could not resolve temporary directory: {error}"))?;
    let actual_parent = fs::canonicalize(parent).map_err(|error| {
        format!(
            "could not resolve staging parent '{}': {error}",
            parent.display()
        )
    })?;
    if actual_parent != expected_parent {
        return Err(format!(
            "refusing to clean staging path outside '{}': '{}'",
            expected_parent.display(),
            directory.display()
        ));
    }
    Ok(())
}

fn cleanup_staging_directory(directory: &Path) -> Result<(), String> {
    if !directory.exists() {
        return Ok(());
    }
    make_staging_directory_writable(directory)?;
    fs::remove_dir_all(directory).map_err(|error| {
        format!(
            "could not remove executable staging directory '{}': {error}",
            directory.display()
        )
    })
}

fn scavenge_stale_staging_directories(parent: &Path, minimum_age: Duration) -> Result<(), String> {
    let entries = fs::read_dir(parent).map_err(|error| {
        format!(
            "could not scan temporary directory '{}' for stale benchmark files: {error}",
            parent.display()
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("could not inspect temporary entry: {error}"))?;
        if !entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(STAGING_DIRECTORY_PREFIX))
        {
            continue;
        }
        let metadata = entry.metadata().map_err(|error| {
            format!(
                "could not inspect stale staging candidate '{}': {error}",
                entry.path().display()
            )
        })?;
        if !metadata.is_dir() {
            continue;
        }
        let modified = metadata.modified().unwrap_or(SystemTime::now());
        let age = SystemTime::now()
            .duration_since(modified)
            .unwrap_or_default();
        if age >= minimum_age {
            let liveness_path = entry.path().join(STAGING_LIVENESS_FILE);
            let liveness_lock = match fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&liveness_path)
            {
                Ok(file) => {
                    if file.try_lock().is_err() {
                        continue;
                    }
                    Some(file)
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(format!(
                        "could not inspect staging liveness marker '{}': {error}",
                        liveness_path.display()
                    ));
                }
            };
            cleanup_staging_directory(&entry.path())?;
            drop(liveness_lock);
        }
    }
    Ok(())
}

fn copy_executable(source: &Path, destination: &Path) -> Result<(), String> {
    let metadata = fs::metadata(source).map_err(|error| {
        format!(
            "could not inspect executable '{}': {error}",
            source.display()
        )
    })?;
    if !metadata.is_file() {
        return Err(format!("executable '{}' is not a file", source.display()));
    }
    fs::copy(source, destination).map_err(|error| {
        format!(
            "could not pin executable '{}' as '{}': {error}",
            source.display(),
            destination.display()
        )
    })?;

    fs::File::open(destination)
        .and_then(|file| file.sync_all())
        .map_err(|error| {
            format!(
                "could not sync pinned executable '{}': {error}",
                destination.display()
            )
        })?;

    let mut permissions = fs::metadata(destination)
        .map_err(|error| {
            format!(
                "could not inspect pinned executable '{}': {error}",
                destination.display()
            )
        })?
        .permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        permissions.set_mode(0o500);
    }
    #[cfg(not(unix))]
    permissions.set_readonly(true);
    fs::set_permissions(destination, permissions).map_err(|error| {
        format!(
            "could not make pinned executable '{}' read-only: {error}",
            destination.display()
        )
    })
}

fn make_staging_directory_read_only(path: &Path) -> Result<(), String> {
    set_staging_directory_permissions(path, true)
}

fn make_staging_directory_writable(path: &Path) -> Result<(), String> {
    set_staging_directory_permissions(path, false)
}

fn set_staging_directory_permissions(path: &Path, read_only: bool) -> Result<(), String> {
    let mut permissions = fs::metadata(path)
        .map_err(|error| {
            format!(
                "could not inspect executable staging directory '{}': {error}",
                path.display()
            )
        })?
        .permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        permissions.set_mode(if read_only { 0o500 } else { 0o700 });
    }
    #[cfg(not(unix))]
    permissions.set_readonly(read_only);
    fs::set_permissions(path, permissions).map_err(|error| {
        format!(
            "could not update executable staging directory '{}': {error}",
            path.display()
        )
    })
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

fn validate_rusthouse(path: &Path, sha256: String) -> Result<RustHouseIdentity, String> {
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
    let mut build_configuration_sha256 = None;
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
            "build_configuration_sha256" => &mut build_configuration_sha256,
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
    let build_configuration_sha256 =
        required_attestation_field(build_configuration_sha256, "build_configuration_sha256")?;
    if target.chars().any(char::is_whitespace) || profile.chars().any(char::is_whitespace) {
        return Err("RustHouse target and profile must not contain whitespace".to_owned());
    }
    if build_configuration_sha256.len() != 64
        || !build_configuration_sha256
            .chars()
            .all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase())
    {
        return Err(
            "RustHouse attestation contains an invalid build_configuration_sha256".to_owned(),
        );
    }

    Ok(RustHouseIdentity {
        sha256,
        source_commit,
        source_dirty,
        rustc_version,
        target,
        profile,
        build_configuration_sha256,
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
        || actual.build_configuration_sha256 != expected.build_configuration_sha256
    {
        return Err(format!(
            "RustHouse build attestation is inconsistent with the benchmark binary: RustHouse commit={} dirty={} rustc={:?} target={} profile={} build_configuration_sha256={}; benchmark commit={} dirty={} rustc={:?} target={} profile={} build_configuration_sha256={}",
            actual.source_commit,
            actual.source_dirty,
            actual.rustc_version,
            actual.target,
            actual.profile,
            actual.build_configuration_sha256,
            expected.source_commit,
            expected.source_dirty,
            expected.rustc_version,
            expected.target,
            expected.profile,
            expected.build_configuration_sha256,
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

fn validate_clickhouse(path: &Path, sha256: String) -> Result<ClickHouseIdentity, String> {
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

    Ok(ClickHouseIdentity {
        version_output,
        sha256,
        artifact_url: CLICKHOUSE_ARTIFACT_URL,
        artifact_platform: CLICKHOUSE_ARTIFACT_PLATFORM,
    })
}

fn validate_executable_sha256(path: &Path, name: &str, expected: &str) -> Result<String, String> {
    if expected.len() != 64
        || !expected
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(format!(
            "{name} expected SHA-256 is unavailable or malformed; rebuild with `cargo run --bin attested-build`"
        ));
    }
    let actual = sha256::file_digest_hex(path)?;
    if actual != expected {
        return Err(format!(
            "{name} checksum mismatch: expected {expected}, got {actual}"
        ));
    }
    Ok(actual)
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
    const BUILD_CONFIGURATION: &str =
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn build_info() -> BuildInfo {
        BuildInfo {
            source_commit: COMMIT,
            source_dirty: false,
            rustc_version: "rustc 1.88.0 (test)",
            target: CLICKHOUSE_TARGET,
            profile: "release",
            build_configuration_sha256: BUILD_CONFIGURATION,
        }
    }

    fn attestation() -> String {
        format!(
            "{ATTESTATION_VERSION}\nsource_commit={COMMIT}\nsource_dirty=false\nrustc_version=rustc 1.88.0 (test)\ntarget={CLICKHOUSE_TARGET}\nprofile=release\nbuild_configuration_sha256={BUILD_CONFIGURATION}\n"
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
    fn mismatched_build_configuration_is_rejected() {
        let tampered = attestation().replace(BUILD_CONFIGURATION, &"b".repeat(64));
        let identity = parse_rusthouse_attestation(&tampered, "a".repeat(64))
            .expect("well-formed attestation");
        let error = validate_rusthouse_build(&identity, build_info())
            .expect_err("different build configuration must fail");
        assert!(error.contains("build_configuration_sha256"));
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

    #[test]
    fn pinned_executables_do_not_follow_source_replacements() {
        let source_directory = env::temp_dir().join(format!(
            "rusthouse-pinning-test-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("unnamed")
        ));
        let _ = fs::remove_dir_all(&source_directory);
        fs::create_dir(&source_directory).expect("source directory");
        let sources = EnginePaths {
            rusthouse: source_directory.join("rusthouse"),
            clickhouse: source_directory.join("clickhouse"),
        };
        fs::write(&sources.rusthouse, b"rusthouse original").expect("rusthouse source");
        fs::write(&sources.clickhouse, b"clickhouse original").expect("clickhouse source");

        let pinned = PinnedExecutables::create(&sources).expect("pinned copies");
        let pinned_paths = pinned.paths();
        fs::write(&sources.rusthouse, b"rusthouse replacement").expect("replace rusthouse");
        fs::write(&sources.clickhouse, b"clickhouse replacement").expect("replace clickhouse");

        assert_eq!(
            fs::read(&pinned_paths.rusthouse).expect("pinned rusthouse"),
            b"rusthouse original"
        );
        assert_eq!(
            fs::read(&pinned_paths.clickhouse).expect("pinned clickhouse"),
            b"clickhouse original"
        );
        let pinned_directory = pinned.directory.clone();
        drop(pinned);
        assert!(!pinned_directory.exists());
        fs::remove_dir_all(source_directory).expect("cleanup sources");
    }

    #[cfg(unix)]
    #[test]
    fn read_only_executables_can_be_pinned() {
        use std::os::unix::fs::PermissionsExt as _;

        let source_directory = env::temp_dir().join(format!(
            "rusthouse-read-only-pinning-test-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("unnamed")
        ));
        let _ = fs::remove_dir_all(&source_directory);
        fs::create_dir(&source_directory).expect("source directory");
        let sources = EnginePaths {
            rusthouse: source_directory.join("rusthouse"),
            clickhouse: source_directory.join("clickhouse"),
        };
        fs::write(&sources.rusthouse, b"read-only rusthouse").expect("rusthouse source");
        fs::write(&sources.clickhouse, b"read-only clickhouse").expect("clickhouse source");
        fs::set_permissions(&sources.rusthouse, fs::Permissions::from_mode(0o555))
            .expect("read-only rusthouse");
        fs::set_permissions(&sources.clickhouse, fs::Permissions::from_mode(0o555))
            .expect("read-only clickhouse");

        let pinned = PinnedExecutables::create(&sources).expect("pin read-only executables");
        let pinned_paths = pinned.paths();
        assert_eq!(
            fs::read(&pinned_paths.rusthouse).expect("pinned rusthouse"),
            b"read-only rusthouse"
        );
        assert_eq!(
            fs::read(&pinned_paths.clickhouse).expect("pinned clickhouse"),
            b"read-only clickhouse"
        );
        drop(pinned);
        fs::remove_dir_all(source_directory).expect("cleanup sources");
    }

    #[cfg(unix)]
    #[test]
    fn checksum_failures_do_not_execute_staged_binaries() {
        let source_directory = env::temp_dir().join(format!(
            "rusthouse-non-execution-test-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("unnamed")
        ));
        let _ = fs::remove_dir_all(&source_directory);
        fs::create_dir(&source_directory).expect("source directory");
        let sources = EnginePaths {
            rusthouse: source_directory.join("rusthouse"),
            clickhouse: source_directory.join("clickhouse"),
        };
        write_marker_script(&sources.rusthouse);
        write_marker_script(&sources.clickhouse);

        let pinned = PinnedExecutables::create(&sources).expect("pinned scripts");
        let paths = pinned.paths();
        let expected_rusthouse = sha256::file_digest_hex(&paths.rusthouse).expect("rusthouse hash");
        let error = paths
            .validate(build_info(), &expected_rusthouse)
            .expect_err("ClickHouse checksum mismatch must fail");
        assert!(error.contains("ClickHouse checksum mismatch"));
        assert!(!marker_path(&paths.rusthouse).exists());
        assert!(!marker_path(&paths.clickhouse).exists());

        let error = paths
            .validate(build_info(), &"b".repeat(64))
            .expect_err("RustHouse checksum mismatch must fail");
        assert!(error.contains("RustHouse checksum mismatch"));
        assert!(!marker_path(&paths.rusthouse).exists());
        assert!(!marker_path(&paths.clickhouse).exists());

        drop(pinned);
        fs::remove_dir_all(source_directory).expect("cleanup sources");
    }

    #[test]
    fn stale_staging_directories_are_scavenged() {
        let parent = env::temp_dir().join(format!(
            "rusthouse-staging-scavenge-test-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("unnamed")
        ));
        let _ = fs::remove_dir_all(&parent);
        fs::create_dir(&parent).expect("scavenge parent");
        let abandoned = parent.join(format!("{STAGING_DIRECTORY_PREFIX}999999-0"));
        let unrelated = parent.join("unrelated-directory");
        fs::create_dir(&abandoned).expect("abandoned staging directory");
        fs::write(
            abandoned.join("clickhouse-pinned"),
            b"large staged artifact",
        )
        .expect("staged artifact");
        make_staging_directory_read_only(&abandoned).expect("read-only abandoned directory");
        fs::create_dir(&unrelated).expect("unrelated directory");

        scavenge_stale_staging_directories(&parent, Duration::ZERO).expect("scavenge staging");
        assert!(!abandoned.exists());
        assert!(unrelated.exists());
        fs::remove_dir_all(parent).expect("cleanup scavenge parent");
    }

    #[test]
    fn active_staging_directory_is_not_scavenged_at_any_age() {
        let parent = env::temp_dir().join(format!(
            "rusthouse-staging-liveness-test-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("unnamed")
        ));
        let _ = fs::remove_dir_all(&parent);
        fs::create_dir(&parent).expect("scavenge parent");
        let active = parent.join(format!("{STAGING_DIRECTORY_PREFIX}999998-0"));
        fs::create_dir(&active).expect("active staging directory");
        let liveness_lock = create_staging_liveness_lock(&active).expect("active liveness lock");
        make_staging_directory_read_only(&active).expect("read-only active directory");

        scavenge_stale_staging_directories(&parent, Duration::ZERO).expect("scavenge staging");
        assert!(active.exists());

        drop(liveness_lock);
        scavenge_stale_staging_directories(&parent, Duration::ZERO)
            .expect("scavenge abandoned staging");
        assert!(!active.exists());
        fs::remove_dir_all(parent).expect("cleanup scavenge parent");
    }

    #[cfg(unix)]
    fn write_marker_script(path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;

        fs::write(path, "#!/bin/sh\ntouch \"${0}.ran\"\n").expect("marker script");
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("executable script");
    }

    #[cfg(unix)]
    fn marker_path(path: &Path) -> PathBuf {
        PathBuf::from(format!("{}.ran", path.display()))
    }
}
