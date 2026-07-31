mod config;
mod dataset;
mod normalize;
mod process;
mod score;
mod sha256;
mod workload;

use std::env;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
#[cfg(test)]
use std::time::Duration;

use config::{Config, ParseResult};
use dataset::Dataset;
use normalize::{ColumnType, compare_outputs};
use process::{Engine, EnginePaths, RunIdentity, TimedBatch, TimedOutput};
use rusthouse::build_info::BuildInfo;
use score::{RatioObservation, ScoreBreakdown, median, parity_score};
use workload::workloads;

const MAX_SAMPLE_SPREAD: f64 = 10.0;

const HELP: &str = "\
RustHouse / ClickHouse Local black-box parity benchmark

USAGE:
    clickhouse-parity-bench [OPTIONS]

OPTIONS:
    --mode <quick|default>  Benchmark size (default: default)
    --quick                 Alias for --mode quick
    --seed <U64>            Deterministic runtime seed (default: 20260729)
    --clickhouse <PATH>     ClickHouse 26.7.1 binary
    --rusthouse <PATH>      Prebuilt rusthouse CLI (default: sibling binary)
    --details <PATH>        Atomically retain attested JSON (required for default)
    -h, --help              Print this help

RUSTHOUSE_CLICKHOUSE_BIN supplies --clickhouse when the flag is absent.
RUSTHOUSE_BIN supplies --rusthouse when the flag is absent.
Build release binaries before benchmarking; compilation is never timed.
";

#[derive(Debug, Default)]
struct TimingSeries {
    rusthouse_batch_ms: Vec<f64>,
    clickhouse_batch_ms: Vec<f64>,
    rusthouse_per_query_ms: Vec<f64>,
    clickhouse_per_query_ms: Vec<f64>,
}

#[derive(Debug)]
struct CaseResult {
    workload: &'static str,
    family: &'static str,
    row_count: usize,
    dataset_seed: u64,
    setup_sql_sha256: String,
    query_sql_sha256: String,
    query_amplification: usize,
    primary: TimingSeries,
    rusthouse_primary_batch_median_ms: f64,
    clickhouse_primary_batch_median_ms: f64,
    rusthouse_primary_median_ms: f64,
    clickhouse_primary_median_ms: f64,
    primary_ratio: f64,
    end_to_end: TimingSeries,
    rusthouse_end_to_end_median_ms: f64,
    clickhouse_end_to_end_median_ms: f64,
    end_to_end_ratio: f64,
}

#[derive(Debug, Default)]
struct CorrectnessGate {
    passed: bool,
}

impl CorrectnessGate {
    fn verify(
        &mut self,
        columns: &[(&str, ColumnType)],
        rusthouse: &TimedOutput,
        clickhouse: &TimedOutput,
    ) -> Result<(), String> {
        compare_outputs(&rusthouse.stdout, &clickhouse.stdout, columns)?;
        self.passed = true;
        Ok(())
    }
}

struct Report {
    score: f64,
    summary: String,
    evidence: Vec<String>,
    suggestions: Vec<String>,
}

struct DetailsContext<'a> {
    config: &'a Config,
    harness: &'a HarnessIdentity,
    identity: &'a RunIdentity,
    cases: &'a [CaseResult],
    primary_score: ScoreBreakdown,
    end_to_end_score: ScoreBreakdown,
    correctness_checks: usize,
    suite_manifest: &'a str,
    suite_manifest_sha256: &'a str,
}

struct HarnessIdentity {
    path: PathBuf,
    sha256: String,
}

fn main() -> ExitCode {
    if let Some(exit_code) = process::run_staging_cleanup_guard_if_requested() {
        return exit_code;
    }
    let default_rusthouse = match default_rusthouse_path() {
        Ok(path) => path,
        Err(error) => return emit_failure(error),
    };
    let parsed = config::parse(
        env::args().skip(1),
        env::var("RUSTHOUSE_CLICKHOUSE_BIN").ok(),
        env::var("RUSTHOUSE_BIN").ok(),
        default_rusthouse,
    );
    let config = match parsed {
        Ok(ParseResult::Help) => {
            print!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Ok(ParseResult::Run(config)) => config,
        Err(error) => return emit_failure(error),
    };

    match run(config) {
        Ok(report) => {
            println!("{}", report.to_json());
            ExitCode::SUCCESS
        }
        Err(error) => emit_failure(error),
    }
}

fn emit_failure(error: String) -> ExitCode {
    let report = Report {
        score: 0.0,
        summary: "Benchmark rejected: no timing score was accepted.".to_owned(),
        evidence: vec![error],
        suggestions: vec![
            "Fix the reported setup or correctness failure and rerun the identical command."
                .to_owned(),
        ],
    };
    println!("{}", report.to_json());
    ExitCode::FAILURE
}

fn default_rusthouse_path() -> Result<PathBuf, String> {
    let executable = env::current_exe()
        .map_err(|error| format!("cannot locate benchmark executable: {error}"))?;
    let directory = executable
        .parent()
        .ok_or_else(|| "benchmark executable has no parent directory".to_owned())?;
    Ok(directory.join(format!("rusthouse{}", env::consts::EXE_SUFFIX)))
}

fn run(config: Config) -> Result<Report, String> {
    let settings = config.mode.settings();
    let build_info = rusthouse::build_info::current(file!())?;
    ensure_default_build(config.mode, build_info)?;
    let harness = harness_identity()?;
    let configured_paths = EnginePaths {
        rusthouse: config.rusthouse.clone(),
        clickhouse: config.clickhouse.clone(),
    };
    if let Some(details) = config.details.as_deref() {
        reject_details_executable_aliases(
            details,
            &[
                ("benchmark", harness.path.as_path()),
                ("RustHouse", configured_paths.rusthouse.as_path()),
                ("ClickHouse", configured_paths.clickhouse.as_path()),
            ],
        )?;
    }
    let expected_rusthouse_sha256 =
        option_env!("RUSTHOUSE_ATTESTED_BINARY_SHA256").unwrap_or("unavailable");
    let (paths, identity, _pinned_executables) =
        configured_paths.pin_and_validate(build_info, expected_rusthouse_sha256)?;
    let mut cases = Vec::new();
    let mut correctness_checks = 0_usize;

    for (row_count_index, row_count) in settings.row_counts.iter().copied().enumerate() {
        let dataset_seed = config.seed ^ (row_count as u64).wrapping_mul(0xd6e8_feb8_6659_fd93);
        let dataset = Dataset::generate(dataset_seed, row_count);
        let setup_sql = dataset.setup_sql();
        let setup_sql_sha256 = sha256::digest_hex(setup_sql.as_bytes());

        for (workload_index, workload) in workloads(row_count).into_iter().enumerate() {
            let query_sql_sha256 = sha256::digest_hex(workload.sql.as_bytes());
            eprintln!(
                "benchmarking {} at {} rows ({}x amplification, {} warmups, {} primary samples, {} end-to-end samples)",
                workload.name,
                row_count,
                settings.query_amplification,
                settings.warmups,
                settings.samples,
                settings.end_to_end_samples
            );

            let correctness_order = (row_count_index + workload_index).is_multiple_of(2);
            let (rusthouse_output, clickhouse_output) =
                execute_correctness_pair(&paths, &setup_sql, &workload.sql, correctness_order)?;
            let mut correctness_gate = CorrectnessGate::default();
            correctness_gate
                .verify(&workload.columns, &rusthouse_output, &clickhouse_output)
                .map_err(|error| {
                    format!(
                        "correctness gate failed for '{}' at {row_count} rows: {error}",
                        workload.name
                    )
                })?;
            correctness_checks += 1;

            let mut primary = TimingSeries::default();
            let primary_iterations = settings.warmups + settings.samples;
            for iteration in 0..primary_iterations {
                let rusthouse_first =
                    (row_count_index + workload_index + iteration + 1).is_multiple_of(2);
                let (rusthouse, clickhouse) = execute_timed_pair(
                    &paths,
                    &setup_sql,
                    &workload.sql,
                    settings.query_amplification,
                    rusthouse_first,
                )?;
                accept_timed_pair(
                    &correctness_gate,
                    &rusthouse,
                    &clickhouse,
                    settings.query_amplification,
                    iteration >= settings.warmups,
                    &mut primary,
                )?;
            }

            let mut end_to_end = TimingSeries::default();
            for iteration in 0..settings.end_to_end_samples {
                let rusthouse_first =
                    (row_count_index + workload_index + iteration + primary_iterations)
                        .is_multiple_of(2);
                let (rusthouse, clickhouse) =
                    execute_timed_pair(&paths, &setup_sql, &workload.sql, 1, rusthouse_first)?;
                accept_timed_pair(
                    &correctness_gate,
                    &rusthouse,
                    &clickhouse,
                    1,
                    true,
                    &mut end_to_end,
                )?;
            }

            let rusthouse_primary_batch_median = stable_median(
                &primary.rusthouse_batch_ms,
                "RustHouse amplified batch",
                workload.name,
                row_count,
            )?;
            let clickhouse_primary_batch_median = stable_median(
                &primary.clickhouse_batch_ms,
                "ClickHouse amplified batch",
                workload.name,
                row_count,
            )?;
            let rusthouse_primary_median = stable_median(
                &primary.rusthouse_per_query_ms,
                "RustHouse amortized query",
                workload.name,
                row_count,
            )?;
            let clickhouse_primary_median = stable_median(
                &primary.clickhouse_per_query_ms,
                "ClickHouse amortized query",
                workload.name,
                row_count,
            )?;
            let rusthouse_end_to_end_median = stable_median(
                &end_to_end.rusthouse_batch_ms,
                "RustHouse end-to-end",
                workload.name,
                row_count,
            )?;
            let clickhouse_end_to_end_median = stable_median(
                &end_to_end.clickhouse_batch_ms,
                "ClickHouse end-to-end",
                workload.name,
                row_count,
            )?;
            let primary_ratio = clickhouse_primary_median / rusthouse_primary_median;
            let end_to_end_ratio = clickhouse_end_to_end_median / rusthouse_end_to_end_median;
            eprintln!(
                "  primary/query: RustHouse {:.3} ms, ClickHouse {:.3} ms, ratio {:.3}; end-to-end ratio {:.3}",
                rusthouse_primary_median,
                clickhouse_primary_median,
                primary_ratio,
                end_to_end_ratio
            );
            cases.push(CaseResult {
                workload: workload.name,
                family: workload.family.name(),
                row_count,
                dataset_seed,
                setup_sql_sha256: setup_sql_sha256.clone(),
                query_sql_sha256,
                query_amplification: settings.query_amplification,
                primary,
                rusthouse_primary_batch_median_ms: rusthouse_primary_batch_median,
                clickhouse_primary_batch_median_ms: clickhouse_primary_batch_median,
                rusthouse_primary_median_ms: rusthouse_primary_median,
                clickhouse_primary_median_ms: clickhouse_primary_median,
                primary_ratio,
                end_to_end,
                rusthouse_end_to_end_median_ms: rusthouse_end_to_end_median,
                clickhouse_end_to_end_median_ms: clickhouse_end_to_end_median,
                end_to_end_ratio,
            });
        }
    }

    paths.revalidate(build_info, expected_rusthouse_sha256, &identity)?;
    revalidate_harness(&harness)?;
    let primary_score = score_cases(&cases, |case| case.primary_ratio)?;
    if config.mode == config::Mode::Default {
        ensure_primary_headroom(&primary_score, cases.len())?;
    }
    let end_to_end_score = score_cases(&cases, |case| case.end_to_end_ratio)?;
    let suite_manifest = suite_manifest_json(&config, &cases);
    let suite_manifest_sha256 = sha256::digest_hex(suite_manifest.as_bytes());
    verify_digest(
        suite_manifest.as_bytes(),
        &suite_manifest_sha256,
        "suite manifest",
    )?;

    if let Some(path) = &config.details {
        let details = details_json(&DetailsContext {
            config: &config,
            harness: &harness,
            identity: &identity,
            cases: &cases,
            primary_score,
            end_to_end_score,
            correctness_checks,
            suite_manifest: &suite_manifest,
            suite_manifest_sha256: &suite_manifest_sha256,
        });
        write_report_atomically(path, details.as_bytes())?;
    }

    let mut evidence = vec![
        format!(
            "{} separate correctness pairs passed across {} cases and {} row counts",
            correctness_checks,
            cases.len(),
            settings.row_counts.len()
        ),
        format!(
            "primary score {:.2}; startup-inclusive end-to-end score {:.2}",
            primary_score.score, end_to_end_score.score
        ),
        format!(
            "primary timing uses setup plus {} identical queries per process, divides positive batch wall time by {}, discards stdout, and performs no startup subtraction",
            settings.query_amplification, settings.query_amplification
        ),
        format!(
            "primary parity caps: {}/{} cases; end-to-end parity caps: {}/{} cases",
            primary_score.saturated_cases,
            cases.len(),
            end_to_end_score.saturated_cases,
            cases.len()
        ),
        format!(
            "mode={}, seed={}, warmups={}, primary_samples={}, end_to_end_samples={}; suite manifest SHA-256={}",
            config.mode.name(),
            config.seed,
            settings.warmups,
            settings.samples,
            settings.end_to_end_samples,
            suite_manifest_sha256
        ),
        format!(
            "RustHouse SHA-256={}; source commit={} dirty={}; rustc={}; target={}; profile={}; build configuration SHA-256={}",
            identity.rusthouse.sha256,
            identity.rusthouse.source_commit,
            identity.rusthouse.source_dirty,
            identity.rusthouse.rustc_version,
            identity.rusthouse.target,
            identity.rusthouse.profile,
            identity.rusthouse.build_configuration_sha256,
        ),
        format!(
            "benchmark harness SHA-256={} ({})",
            harness.sha256,
            harness.path.display()
        ),
        format!(
            "ClickHouse identity: {}; SHA-256={}; artifact={} ({})",
            identity.clickhouse.version_output,
            identity.clickhouse.sha256,
            identity.clickhouse.artifact_url,
            identity.clickhouse.artifact_platform,
        ),
        format!(
            "host platform: {} ({})",
            identity.host.platform, identity.host.description
        ),
        format!(
            "limitation: amplification measures repeated warm in-process work, retains 1/{} of startup/setup, and does not model concurrency, durable storage, or network access",
            settings.query_amplification
        ),
    ];
    evidence.extend(cases.iter().map(|case| {
        format!(
            "{} / {} rows: primary/query RustHouse {:.3} ms, ClickHouse {:.3} ms, ratio {:.3}; end-to-end RustHouse {:.3} ms, ClickHouse {:.3} ms, ratio {:.3}",
            case.workload,
            case.row_count,
            case.rusthouse_primary_median_ms,
            case.clickhouse_primary_median_ms,
            case.primary_ratio,
            case.rusthouse_end_to_end_median_ms,
            case.clickhouse_end_to_end_median_ms,
            case.end_to_end_ratio
        )
    }));
    let suggestions = if config.mode == config::Mode::Quick {
        vec![
            "Use --mode default for the decision-grade 1k/10k/50k-row suite.".to_owned(),
            "Rerun with several explicit --seed values before drawing optimization conclusions."
                .to_owned(),
        ]
    } else {
        vec![
            "Repeat the default suite with several explicit seeds and compare detailed case medians."
                .to_owned(),
            "Treat regressions in correctness as score zero, regardless of timing improvements."
                .to_owned(),
        ]
    };

    Ok(Report {
        score: primary_score.score,
        summary: format!(
            "RustHouse primary sustained-work score {:.2}; startup-inclusive end-to-end score {:.2}; ClickHouse parity=100 over {} correctness-gated cases.",
            primary_score.score,
            end_to_end_score.score,
            cases.len()
        ),
        evidence,
        suggestions,
    })
}

fn ensure_default_build(mode: config::Mode, build_info: BuildInfo) -> Result<(), String> {
    if mode != config::Mode::Default {
        return Ok(());
    }
    if build_info.profile != "release" {
        return Err(format!(
            "default mode requires release binaries, got profile {:?}",
            build_info.profile
        ));
    }
    if build_info.source_dirty {
        return Err(
            "default mode requires clean sources so the embedded commit identifies one source tree"
                .to_owned(),
        );
    }
    Ok(())
}

fn harness_identity() -> Result<HarnessIdentity, String> {
    let path = env::current_exe()
        .map_err(|error| format!("cannot locate benchmark executable: {error}"))?;
    let sha256 = sha256::file_digest_hex(&path)?;
    Ok(HarnessIdentity { path, sha256 })
}

fn revalidate_harness(expected: &HarnessIdentity) -> Result<(), String> {
    let actual = sha256::file_digest_hex(&expected.path)?;
    if actual != expected.sha256 {
        return Err(
            "benchmark harness changed while the suite was running; no report was retained"
                .to_owned(),
        );
    }
    Ok(())
}

fn reject_details_executable_aliases(
    details: &Path,
    executables: &[(&str, &Path)],
) -> Result<(), String> {
    for (name, executable) in executables {
        if paths_refer_to_same_file(details, executable)? {
            return Err(format!(
                "details path '{}' aliases the {name} executable '{}'; refusing to overwrite a benchmark input",
                details.display(),
                executable.display()
            ));
        }
    }
    Ok(())
}

fn paths_refer_to_same_file(left: &Path, right: &Path) -> Result<bool, String> {
    let left_resolved = resolve_existing_or_parent(left)?;
    let right_resolved = fs::canonicalize(right).map_err(|error| {
        format!(
            "could not resolve executable path '{}': {error}",
            right.display()
        )
    })?;
    if left_resolved == right_resolved {
        return Ok(true);
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        let left_metadata = match fs::metadata(left) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(format!(
                    "could not inspect details path '{}': {error}",
                    left.display()
                ));
            }
        };
        let right_metadata = fs::metadata(right).map_err(|error| {
            format!(
                "could not inspect executable path '{}': {error}",
                right.display()
            )
        })?;
        if let Some(left_metadata) = left_metadata {
            return Ok(left_metadata.dev() == right_metadata.dev()
                && left_metadata.ino() == right_metadata.ino());
        }
    }

    Ok(false)
}

fn resolve_existing_or_parent(path: &Path) -> Result<PathBuf, String> {
    match fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let file_name = path
                .file_name()
                .ok_or_else(|| format!("details path '{}' has no file name", path.display()))?;
            fs::canonicalize(parent)
                .map(|parent| parent.join(file_name))
                .map_err(|error| {
                    format!(
                        "could not resolve details directory '{}': {error}",
                        parent.display()
                    )
                })
        }
        Err(error) => Err(format!(
            "could not resolve details path '{}': {error}",
            path.display()
        )),
    }
}

fn score_cases(
    cases: &[CaseResult],
    ratio: impl Fn(&CaseResult) -> f64,
) -> Result<ScoreBreakdown, String> {
    let observations = cases
        .iter()
        .map(|case| RatioObservation {
            family: case.family,
            scale: case.row_count,
            ratio: ratio(case),
        })
        .collect::<Vec<_>>();
    parity_score(&observations)
}

fn ensure_primary_headroom(score: &ScoreBreakdown, case_count: usize) -> Result<(), String> {
    if score.saturated_cases == case_count {
        return Err(
            "primary timing saturated: every case reached the parity cap; increase query amplification before accepting this benchmark"
                .to_owned(),
        );
    }
    Ok(())
}

fn execute_correctness_pair(
    paths: &EnginePaths,
    setup_sql: &str,
    query_sql: &str,
    rusthouse_first: bool,
) -> Result<(TimedOutput, TimedOutput), String> {
    if rusthouse_first {
        let rusthouse = paths.execute_correctness(Engine::RustHouse, setup_sql, query_sql)?;
        let clickhouse = paths.execute_correctness(Engine::ClickHouse, setup_sql, query_sql)?;
        Ok((rusthouse, clickhouse))
    } else {
        let clickhouse = paths.execute_correctness(Engine::ClickHouse, setup_sql, query_sql)?;
        let rusthouse = paths.execute_correctness(Engine::RustHouse, setup_sql, query_sql)?;
        Ok((rusthouse, clickhouse))
    }
}

fn execute_timed_pair(
    paths: &EnginePaths,
    setup_sql: &str,
    query_sql: &str,
    query_repetitions: usize,
    rusthouse_first: bool,
) -> Result<(TimedBatch, TimedBatch), String> {
    if rusthouse_first {
        let rusthouse =
            paths.execute_timed(Engine::RustHouse, setup_sql, query_sql, query_repetitions)?;
        let clickhouse =
            paths.execute_timed(Engine::ClickHouse, setup_sql, query_sql, query_repetitions)?;
        Ok((rusthouse, clickhouse))
    } else {
        let clickhouse =
            paths.execute_timed(Engine::ClickHouse, setup_sql, query_sql, query_repetitions)?;
        let rusthouse =
            paths.execute_timed(Engine::RustHouse, setup_sql, query_sql, query_repetitions)?;
        Ok((rusthouse, clickhouse))
    }
}

fn accept_timed_pair(
    gate: &CorrectnessGate,
    rusthouse: &TimedBatch,
    clickhouse: &TimedBatch,
    expected_repetitions: usize,
    record: bool,
    samples: &mut TimingSeries,
) -> Result<(), String> {
    if !gate.passed {
        return Err("timed batch was not preceded by a passing correctness run".to_owned());
    }
    if rusthouse.query_repetitions != clickhouse.query_repetitions
        || rusthouse.query_repetitions != expected_repetitions
    {
        return Err(format!(
            "query amplification mismatch: expected {expected_repetitions}, RustHouse used {}, ClickHouse used {}",
            rusthouse.query_repetitions, clickhouse.query_repetitions
        ));
    }

    let rusthouse_batch_ms = rusthouse.elapsed.as_secs_f64() * 1_000.0;
    let clickhouse_batch_ms = clickhouse.elapsed.as_secs_f64() * 1_000.0;
    let rusthouse_per_query_ms = per_query_millis(rusthouse_batch_ms, rusthouse.query_repetitions)?;
    let clickhouse_per_query_ms =
        per_query_millis(clickhouse_batch_ms, clickhouse.query_repetitions)?;

    if record {
        samples.rusthouse_batch_ms.push(rusthouse_batch_ms);
        samples.clickhouse_batch_ms.push(clickhouse_batch_ms);
        samples.rusthouse_per_query_ms.push(rusthouse_per_query_ms);
        samples
            .clickhouse_per_query_ms
            .push(clickhouse_per_query_ms);
    }
    Ok(())
}

fn per_query_millis(batch_millis: f64, query_repetitions: usize) -> Result<f64, String> {
    if query_repetitions == 0 {
        return Err("query amplification must be positive".to_owned());
    }
    if !batch_millis.is_finite() || batch_millis <= 0.0 {
        return Err(format!(
            "timed batch duration must be finite and positive, got {batch_millis}"
        ));
    }
    let per_query = batch_millis / query_repetitions as f64;
    if !per_query.is_finite() || per_query <= 0.0 {
        return Err(format!(
            "amortized query duration must be finite and positive, got {per_query}"
        ));
    }
    Ok(per_query)
}

fn stable_median(
    samples: &[f64],
    engine_metric: &str,
    workload: &str,
    row_count: usize,
) -> Result<f64, String> {
    let value = median(samples)?;
    if value <= 0.0 {
        return Err(format!(
            "timer resolution was insufficient for {engine_metric}, '{workload}' at {row_count} rows"
        ));
    }
    let minimum = samples.iter().copied().fold(f64::INFINITY, f64::min);
    let maximum = samples.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if maximum / minimum > MAX_SAMPLE_SPREAD {
        return Err(format!(
            "unstable timing for {engine_metric}, '{workload}' at {row_count} rows: max/min spread {:.2} exceeds {:.2}",
            maximum / minimum,
            MAX_SAMPLE_SPREAD
        ));
    }
    Ok(value)
}

fn suite_manifest_json(config: &Config, cases: &[CaseResult]) -> String {
    let settings = config.mode.settings();
    let mut output = String::new();
    write!(
        output,
        "{{\"manifest_version\":1,\"mode\":{},\"seed\":{},\"warmups\":{},\"primary_samples\":{},\"end_to_end_samples\":{},\"query_amplification\":{},\"max_sample_spread\":{MAX_SAMPLE_SPREAD:.1},\"row_counts\":[",
        json_string(config.mode.name()),
        config.seed,
        settings.warmups,
        settings.samples,
        settings.end_to_end_samples,
        settings.query_amplification,
    )
    .expect("writing to String cannot fail");
    for (index, row_count) in settings.row_counts.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        write!(output, "{row_count}").expect("writing to String cannot fail");
    }
    output.push_str("],\"cases\":[");
    for (index, case) in cases.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        write!(
            output,
            "{{\"workload\":{},\"family\":{},\"row_count\":{},\"dataset_seed\":{},\"setup_sql_sha256\":{},\"query_sql_sha256\":{}}}",
            json_string(case.workload),
            json_string(case.family),
            case.row_count,
            case.dataset_seed,
            json_string(&case.setup_sql_sha256),
            json_string(&case.query_sql_sha256),
        )
        .expect("writing to String cannot fail");
    }
    output.push_str("]}");
    output
}

fn verify_digest(bytes: &[u8], expected: &str, subject: &str) -> Result<(), String> {
    let actual = sha256::digest_hex(bytes);
    if actual != expected {
        return Err(format!(
            "{subject} SHA-256 mismatch: expected {expected}, got {actual}"
        ));
    }
    Ok(())
}

fn details_json(context: &DetailsContext<'_>) -> String {
    let DetailsContext {
        config,
        harness,
        identity,
        cases,
        primary_score,
        end_to_end_score,
        correctness_checks,
        suite_manifest,
        suite_manifest_sha256,
    } = context;
    let settings = config.mode.settings();
    let mut output = String::new();
    write!(
        output,
        "{{\"schema_version\":5,\"score\":{:.6},\"primary_score\":{:.6},\"end_to_end_score\":{:.6},\"primary_saturated_cases\":{},\"end_to_end_saturated_cases\":{},\"mode\":{},\"seed\":{},\"warmups\":{},\"primary_samples\":{},\"end_to_end_samples\":{},\"row_counts\":[",
        primary_score.score,
        primary_score.score,
        end_to_end_score.score,
        primary_score.saturated_cases,
        end_to_end_score.saturated_cases,
        json_string(config.mode.name()),
        config.seed,
        settings.warmups,
        settings.samples,
        settings.end_to_end_samples
    )
    .expect("writing to String cannot fail");
    for (index, row_count) in settings.row_counts.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        write!(output, "{row_count}").expect("writing to String cannot fail");
    }
    write!(
        output,
        "],\"timing_method\":{{\"name\":\"in_process_query_amplification\",\"calibration\":\"fixed_shared_repetitions\",\"query_amplification\":{},\"startup_subtraction\":false,\"correctness_runs_separate\":true,\"max_sample_spread\":{MAX_SAMPLE_SPREAD:.1}}},\"correctness_checks\":{correctness_checks},\"benchmark\":{{\"path\":{},\"sha256\":{}}},\"rusthouse\":{{\"path\":{},\"sha256\":{},\"source_commit\":{},\"source_dirty\":{},\"rustc_version\":{},\"target\":{},\"profile\":{},\"build_configuration_sha256\":{}}},\"clickhouse\":{{\"path\":{},\"version\":{},\"sha256\":{},\"artifact_url\":{},\"artifact_platform\":{}}},\"host\":{{\"platform\":{},\"description\":{}}},\"suite_manifest_sha256\":{},\"suite_manifest\":{},\"limitations\":[{},{}],\"cases\":[",
        settings.query_amplification,
        json_string(&harness.path.display().to_string()),
        json_string(&harness.sha256),
        json_string(&config.rusthouse.display().to_string()),
        json_string(&identity.rusthouse.sha256),
        json_string(&identity.rusthouse.source_commit),
        identity.rusthouse.source_dirty,
        json_string(&identity.rusthouse.rustc_version),
        json_string(&identity.rusthouse.target),
        json_string(&identity.rusthouse.profile),
        json_string(&identity.rusthouse.build_configuration_sha256),
        json_string(&config.clickhouse.display().to_string()),
        json_string(&identity.clickhouse.version_output),
        json_string(&identity.clickhouse.sha256),
        json_string(identity.clickhouse.artifact_url),
        json_string(identity.clickhouse.artifact_platform),
        json_string(&identity.host.platform),
        json_string(&identity.host.description),
        json_string(suite_manifest_sha256),
        suite_manifest,
        json_string("amplification measures repeated warm in-process work and retains one divided by the amplification factor of startup and setup"),
        json_string("synthetic single-process data does not model concurrency, durable storage, networking, joins, nullability, or production compression")
    )
    .expect("writing to String cannot fail");

    for (index, case) in cases.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        write!(
            output,
            "{{\"workload\":{},\"family\":{},\"row_count\":{},\"dataset_seed\":{},\"setup_sql_sha256\":{},\"query_sql_sha256\":{},\"query_amplification\":{},\"primary\":{{\"rusthouse_batch_median_ms\":{:.6},\"clickhouse_batch_median_ms\":{:.6},\"rusthouse_per_query_median_ms\":{:.6},\"clickhouse_per_query_median_ms\":{:.6},\"clickhouse_rusthouse_ratio\":{:.9},\"rusthouse_batch_samples_ms\":",
            json_string(case.workload),
            json_string(case.family),
            case.row_count,
            case.dataset_seed,
            json_string(&case.setup_sql_sha256),
            json_string(&case.query_sql_sha256),
            case.query_amplification,
            case.rusthouse_primary_batch_median_ms,
            case.clickhouse_primary_batch_median_ms,
            case.rusthouse_primary_median_ms,
            case.clickhouse_primary_median_ms,
            case.primary_ratio
        )
        .expect("writing to String cannot fail");
        write_number_array(&mut output, &case.primary.rusthouse_batch_ms);
        output.push_str(",\"clickhouse_batch_samples_ms\":");
        write_number_array(&mut output, &case.primary.clickhouse_batch_ms);
        output.push_str(",\"rusthouse_per_query_samples_ms\":");
        write_number_array(&mut output, &case.primary.rusthouse_per_query_ms);
        output.push_str(",\"clickhouse_per_query_samples_ms\":");
        write_number_array(&mut output, &case.primary.clickhouse_per_query_ms);
        write!(
            output,
            "}},\"end_to_end\":{{\"rusthouse_median_ms\":{:.6},\"clickhouse_median_ms\":{:.6},\"clickhouse_rusthouse_ratio\":{:.9},\"rusthouse_samples_ms\":",
            case.rusthouse_end_to_end_median_ms,
            case.clickhouse_end_to_end_median_ms,
            case.end_to_end_ratio
        )
        .expect("writing to String cannot fail");
        write_number_array(&mut output, &case.end_to_end.rusthouse_batch_ms);
        output.push_str(",\"clickhouse_samples_ms\":");
        write_number_array(&mut output, &case.end_to_end.clickhouse_batch_ms);
        output.push_str("}}");
    }
    output.push_str("]}\n");
    output
}

fn write_report_atomically(path: &Path, contents: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| format!("details path '{}' has no file name", path.display()))?;

    let mut temporary_path = None;
    let mut temporary_file = None;
    for attempt in 0..100_u32 {
        let candidate = parent.join(format!(
            ".{file_name}.{}.{}.tmp",
            std::process::id(),
            attempt
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                temporary_path = Some(candidate);
                temporary_file = Some(file);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "could not create atomic details file in '{}': {error}",
                    parent.display()
                ));
            }
        }
    }

    let temporary_path = temporary_path.ok_or_else(|| {
        format!(
            "could not reserve an atomic details file in '{}'",
            parent.display()
        )
    })?;
    let temporary_file = temporary_file.expect("path and file are set together");
    let result = install_atomic_report(temporary_file, &temporary_path, path, parent, contents);
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

fn install_atomic_report(
    temporary_file: fs::File,
    temporary_path: &Path,
    destination: &Path,
    parent: &Path,
    contents: &[u8],
) -> Result<(), String> {
    install_atomic_report_with_sync(
        temporary_file,
        temporary_path,
        destination,
        parent,
        contents,
        sync_directory,
    )
}

fn install_atomic_report_with_sync<F>(
    mut temporary_file: fs::File,
    temporary_path: &Path,
    destination: &Path,
    parent: &Path,
    contents: &[u8],
    mut sync_parent: F,
) -> Result<(), String>
where
    F: FnMut(&Path) -> io::Result<()>,
{
    temporary_file.write_all(contents).map_err(|error| {
        format!(
            "could not write atomic details file '{}': {error}",
            temporary_path.display()
        )
    })?;
    temporary_file.sync_all().map_err(|error| {
        format!(
            "could not sync atomic details file '{}': {error}",
            temporary_path.display()
        )
    })?;
    let installed_identity = report_file_identity(&temporary_file.metadata().map_err(|error| {
        format!(
            "could not inspect atomic details file '{}': {error}",
            temporary_path.display()
        )
    })?);
    drop(temporary_file);
    let backup = backup_existing_report(destination, parent)?;
    if let Err(error) = fs::rename(temporary_path, destination) {
        if let Some(backup) = backup {
            let _ = fs::remove_file(backup);
        }
        return Err(format!(
            "could not atomically replace details file '{}': {error}",
            destination.display()
        ));
    }
    if let Err(error) = sync_parent(parent) {
        let still_installed = installed_identity
            .ok_or_else(|| {
                format!(
                    "could not sync details directory '{}': {error}; refused unsafe rollback because file identity is unavailable on this platform",
                    parent.display()
                )
            })
            .and_then(|installed_identity| {
                fs::metadata(destination)
                    .map_err(|identity_error| {
                        format!(
                            "could not sync details directory '{}': {error}; refused unsafe rollback because details file '{}' could not be inspected: {identity_error}",
                            parent.display(),
                            destination.display()
                        )
                    })
                    .and_then(|metadata| {
                        report_file_identity(&metadata)
                            .ok_or_else(|| "details file identity became unavailable".to_owned())
                            .map(|identity| identity == installed_identity)
                    })
            });
        match still_installed {
            Ok(true) => {}
            Ok(false) => {
                let cleanup = remove_report_backup(backup.as_deref());
                let mut message = format!(
                    "could not sync details directory '{}': {error}; details file changed concurrently, so the newer report was preserved",
                    parent.display()
                );
                if let Err(cleanup_error) = cleanup {
                    write!(
                        message,
                        "; could not remove obsolete report backup: {cleanup_error}"
                    )
                    .expect("writing to String cannot fail");
                }
                return Err(message);
            }
            Err(identity_error) => {
                let cleanup = remove_report_backup(backup.as_deref());
                let mut message = identity_error;
                if let Err(cleanup_error) = cleanup {
                    write!(
                        message,
                        "; could not remove obsolete report backup: {cleanup_error}"
                    )
                    .expect("writing to String cannot fail");
                }
                return Err(message);
            }
        }
        let rollback = restore_previous_report(destination, backup.as_deref());
        let rollback_sync = rollback
            .as_ref()
            .ok()
            .and_then(|_| sync_parent(parent).err());
        let mut message = format!(
            "could not sync details directory '{}': {error}",
            parent.display()
        );
        if let Err(rollback_error) = rollback {
            write!(
                message,
                "; could not roll back rejected report: {rollback_error}"
            )
            .expect("writing to String cannot fail");
        } else if let Some(rollback_sync_error) = rollback_sync {
            write!(
                message,
                "; rolled back report but could not sync rollback: {rollback_sync_error}"
            )
            .expect("writing to String cannot fail");
        }
        return Err(message);
    }
    if let Some(backup) = backup {
        let _ = fs::remove_file(backup);
        let _ = sync_parent(parent);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReportFileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

fn report_file_identity(metadata: &fs::Metadata) -> Option<ReportFileIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        Some(ReportFileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

fn remove_report_backup(backup: Option<&Path>) -> io::Result<()> {
    let Some(backup) = backup else {
        return Ok(());
    };
    match fs::remove_file(backup) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn backup_existing_report(destination: &Path, parent: &Path) -> Result<Option<PathBuf>, String> {
    if !destination.exists() {
        return Ok(None);
    }
    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("details path '{}' has no file name", destination.display()))?;
    for attempt in 0..100_u32 {
        let backup = parent.join(format!(
            ".{file_name}.{}.{}.backup",
            std::process::id(),
            attempt
        ));
        match fs::hard_link(destination, &backup) {
            Ok(()) => return Ok(Some(backup)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "could not retain previous details file '{}': {error}",
                    destination.display()
                ));
            }
        }
    }
    Err(format!(
        "could not reserve a details backup in '{}'",
        parent.display()
    ))
}

fn restore_previous_report(destination: &Path, backup: Option<&Path>) -> io::Result<()> {
    let Some(backup) = backup else {
        return match fs::remove_file(destination) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
    };
    match fs::rename(backup, destination) {
        Ok(()) => Ok(()),
        Err(_) => {
            fs::remove_file(destination)?;
            fs::rename(backup, destination)
        }
    }
}

fn sync_directory(path: &Path) -> io::Result<()> {
    fs::File::open(path).and_then(|directory| directory.sync_all())
}

fn write_number_array(output: &mut String, values: &[f64]) {
    output.push('[');
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        write!(output, "{value:.6}").expect("writing to String cannot fail");
    }
    output.push(']');
}

impl Report {
    fn to_json(&self) -> String {
        let mut output = format!(
            "{{\"score\":{:.6},\"summary\":{},\"evidence\":[",
            self.score,
            json_string(&self.summary)
        );
        write_string_array(&mut output, &self.evidence);
        output.push_str("],\"suggestions\":[");
        write_string_array(&mut output, &self.suggestions);
        output.push_str("]}");
        output
    }
}

fn write_string_array(output: &mut String, values: &[String]) {
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        output.push_str(&json_string(value));
    }
}

fn json_string(value: &str) -> String {
    let mut output = String::from("\"");
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            value if value.is_control() => {
                write!(output, "\\u{:04x}", value as u32).expect("writing to String cannot fail");
            }
            value => output.push(value),
        }
    }
    output.push('"');
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(milliseconds: f64, repetitions: usize) -> TimedBatch {
        TimedBatch {
            elapsed: Duration::from_secs_f64(milliseconds / 1_000.0),
            query_repetitions: repetitions,
        }
    }

    fn output(csv: &str) -> TimedOutput {
        TimedOutput {
            stdout: csv.to_owned(),
        }
    }

    #[test]
    fn correctness_failure_leaves_timing_gate_closed() {
        let mut gate = CorrectnessGate::default();
        let error = gate
            .verify(
                &[("n", ColumnType::Integer)],
                &output("n\n1\n"),
                &output("n\n2\n"),
            )
            .expect_err("mismatch must fail");

        assert!(error.contains("result mismatch"));
        assert!(!gate.passed);
    }

    #[test]
    fn timed_batches_require_a_correctness_gate() {
        let mut samples = TimingSeries::default();
        let error = accept_timed_pair(
            &CorrectnessGate::default(),
            &batch(10.0, 64),
            &batch(10.0, 64),
            64,
            true,
            &mut samples,
        )
        .expect_err("ungated timing must fail");

        assert!(error.contains("correctness"));
        assert!(samples.rusthouse_batch_ms.is_empty());
        assert!(samples.clickhouse_batch_ms.is_empty());
    }

    #[test]
    fn amplification_must_match_for_both_engines() {
        let mut samples = TimingSeries::default();
        let error = accept_timed_pair(
            &CorrectnessGate { passed: true },
            &batch(10.0, 64),
            &batch(10.0, 63),
            64,
            true,
            &mut samples,
        )
        .expect_err("different amplification must fail");

        assert!(error.contains("amplification mismatch"));
        assert!(samples.rusthouse_batch_ms.is_empty());
        assert!(samples.clickhouse_batch_ms.is_empty());
    }

    #[test]
    fn amortization_rejects_zero_negative_and_non_finite_timings() {
        assert!(per_query_millis(0.0, 64).is_err());
        assert!(per_query_millis(-1.0, 64).is_err());
        assert!(per_query_millis(f64::NAN, 64).is_err());
        assert!(per_query_millis(1.0, 0).is_err());
    }

    #[test]
    fn unstable_samples_are_rejected() {
        let error = stable_median(&[1.0, 1.1, 20.0], "engine", "workload", 10)
            .expect_err("large spread must fail");
        assert!(error.contains("unstable timing"));
    }

    #[test]
    fn normalized_match_opens_gate_and_accepts_positive_timing() {
        let mut gate = CorrectnessGate::default();
        gate.verify(
            &[("enabled", ColumnType::Boolean)],
            &output("enabled\ntrue\n"),
            &output("enabled\n1\n"),
        )
        .expect("matching output");

        let mut samples = TimingSeries::default();
        accept_timed_pair(
            &gate,
            &batch(64.0, 64),
            &batch(32.0, 64),
            64,
            true,
            &mut samples,
        )
        .expect("gated sample");
        assert_eq!(samples.rusthouse_per_query_ms, [1.0]);
        assert_eq!(samples.clickhouse_per_query_ms, [0.5]);
    }

    #[test]
    fn a_fully_capped_primary_score_is_rejected() {
        let score = ScoreBreakdown {
            score: 100.0,
            saturated_cases: 8,
        };
        assert!(ensure_primary_headroom(&score, 8).is_err());
    }

    #[test]
    fn default_mode_rejects_dirty_or_non_release_builds() {
        let release = BuildInfo {
            source_commit: "0123456789abcdef0123456789abcdef01234567",
            source_dirty: false,
            rustc_version: "rustc test",
            target: "aarch64-apple-darwin",
            profile: "release",
            build_configuration_sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        };
        ensure_default_build(config::Mode::Default, release).expect("clean release build");
        ensure_default_build(
            config::Mode::Quick,
            BuildInfo {
                source_dirty: true,
                ..release
            },
        )
        .expect("quick mode permits dirty development builds");

        let dirty_error = ensure_default_build(
            config::Mode::Default,
            BuildInfo {
                source_dirty: true,
                ..release
            },
        )
        .expect_err("dirty default build must fail");
        assert!(dirty_error.contains("clean sources"));

        let profile_error = ensure_default_build(
            config::Mode::Default,
            BuildInfo {
                profile: "debug",
                ..release
            },
        )
        .expect_err("debug default build must fail");
        assert!(profile_error.contains("release binaries"));
    }

    #[test]
    fn benchmark_harness_digest_revalidates() {
        let identity = harness_identity().expect("harness identity");
        revalidate_harness(&identity).expect("unchanged harness");
    }

    #[test]
    fn burner_report_is_one_compact_object_with_required_fields() {
        let report = Report {
            score: 10.0,
            summary: "summary".to_owned(),
            evidence: vec!["evidence".to_owned()],
            suggestions: vec!["suggestion".to_owned()],
        }
        .to_json();

        assert_eq!(
            report,
            "{\"score\":10.000000,\"summary\":\"summary\",\"evidence\":[\"evidence\"],\"suggestions\":[\"suggestion\"]}"
        );
        assert!(!report.contains('\n'));
    }

    #[test]
    fn suite_manifest_digest_rejects_tampering() {
        let manifest = br#"{"manifest_version":1,"cases":[]}"#;
        let digest = sha256::digest_hex(manifest);
        verify_digest(manifest, &digest, "suite manifest").expect("original manifest");

        let tampered = br#"{"manifest_version":1,"cases":[{}]}"#;
        let error = verify_digest(tampered, &digest, "suite manifest")
            .expect_err("tampered manifest must fail");
        assert!(error.contains("mismatch"));
    }

    #[test]
    fn details_report_replaces_existing_file_atomically() {
        let directory = env::temp_dir().join(format!(
            "rusthouse-atomic-report-test-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("unnamed")
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir(&directory).expect("test directory");
        let path = directory.join("details.json");
        fs::write(&path, b"old").expect("old report");

        write_report_atomically(&path, b"new report\n").expect("atomic report");
        assert_eq!(fs::read(&path).expect("retained report"), b"new report\n");
        assert_eq!(fs::read_dir(&directory).expect("directory").count(), 1);
        fs::remove_dir_all(directory).expect("cleanup");
    }

    #[test]
    fn details_path_cannot_alias_benchmark_inputs() {
        let directory = env::temp_dir().join(format!(
            "rusthouse-details-alias-test-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("unnamed")
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir(&directory).expect("test directory");
        let harness = directory.join("benchmark");
        let rusthouse = directory.join("rusthouse");
        let clickhouse = directory.join("clickhouse");
        fs::write(&harness, b"benchmark").expect("benchmark");
        fs::write(&rusthouse, b"rusthouse").expect("rusthouse");
        fs::write(&clickhouse, b"clickhouse").expect("clickhouse");
        let executables = [
            ("benchmark", harness.as_path()),
            ("RustHouse", rusthouse.as_path()),
            ("ClickHouse", clickhouse.as_path()),
        ];

        assert!(reject_details_executable_aliases(&rusthouse, &executables).is_err());
        reject_details_executable_aliases(&directory.join("details.json"), &executables)
            .expect("distinct details path");

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let symlink_path = directory.join("details-symlink.json");
            symlink(&clickhouse, &symlink_path).expect("details symlink");
            assert!(reject_details_executable_aliases(&symlink_path, &executables).is_err());

            let hard_link_path = directory.join("details-hard-link.json");
            fs::hard_link(&harness, &hard_link_path).expect("details hard link");
            assert!(reject_details_executable_aliases(&hard_link_path, &executables).is_err());
        }

        fs::remove_dir_all(directory).expect("cleanup");
    }

    #[test]
    fn directory_sync_failure_rolls_back_rejected_report() {
        let directory = env::temp_dir().join(format!(
            "rusthouse-atomic-rollback-test-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("unnamed")
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir(&directory).expect("test directory");
        let destination = directory.join("details.json");
        fs::write(&destination, b"old report\n").expect("old report");
        let temporary_path = directory.join(".details.tmp");
        let temporary_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
            .expect("temporary report");
        let mut sync_attempt = 0_u32;

        let error = install_atomic_report_with_sync(
            temporary_file,
            &temporary_path,
            &destination,
            &directory,
            b"rejected report\n",
            |_| {
                sync_attempt += 1;
                if sync_attempt == 1 {
                    Err(io::Error::other("injected directory sync failure"))
                } else {
                    Ok(())
                }
            },
        )
        .expect_err("directory sync failure must reject report");

        assert!(error.contains("injected directory sync failure"));
        assert_eq!(
            fs::read(&destination).expect("restored report"),
            b"old report\n"
        );
        assert_eq!(fs::read_dir(&directory).expect("directory").count(), 1);

        let new_destination = directory.join("new-details.json");
        let new_temporary_path = directory.join(".new-details.tmp");
        let new_temporary_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&new_temporary_path)
            .expect("new temporary report");
        let error = install_atomic_report_with_sync(
            new_temporary_file,
            &new_temporary_path,
            &new_destination,
            &directory,
            b"rejected report\n",
            |_| Err(io::Error::other("injected directory sync failure")),
        )
        .expect_err("directory sync failure must reject new report");
        assert!(error.contains("injected directory sync failure"));
        assert!(!new_destination.exists());
        assert_eq!(fs::read_dir(&directory).expect("directory").count(), 1);
        fs::remove_dir_all(directory).expect("cleanup");
    }

    #[test]
    fn failed_writer_does_not_roll_back_a_concurrent_report() {
        let directory = env::temp_dir().join(format!(
            "rusthouse-atomic-concurrent-rollback-test-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("unnamed")
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir(&directory).expect("test directory");
        let destination = directory.join("details.json");
        fs::write(&destination, b"old report\n").expect("old report");
        let temporary_path = directory.join(".details.tmp");
        let temporary_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
            .expect("temporary report");
        let error = install_atomic_report_with_sync(
            temporary_file,
            &temporary_path,
            &destination,
            &directory,
            b"rejected report\n",
            |_| {
                write_report_atomically(&destination, b"accepted concurrent report\n")
                    .map_err(io::Error::other)?;
                Err(io::Error::other("injected first-writer sync failure"))
            },
        )
        .expect_err("first writer must reject its report");

        assert!(error.contains("changed concurrently"));
        assert_eq!(
            fs::read(&destination).expect("concurrent report"),
            b"accepted concurrent report\n"
        );
        assert_eq!(fs::read_dir(&directory).expect("directory").count(), 1);
        fs::remove_dir_all(directory).expect("cleanup");
    }
}
