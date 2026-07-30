mod config;
mod dataset;
mod normalize;
mod process;
mod score;
mod workload;

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;
#[cfg(test)]
use std::time::Duration;

use config::{Config, ParseResult};
use dataset::Dataset;
use normalize::{ColumnType, compare_outputs};
use process::{ClickHouseIdentity, Engine, EnginePaths, TimedBatch, TimedOutput};
use score::{ExpectedCase, RatioObservation, ScoreBreakdown, ScorePanel, median, parity_score};
use workload::workloads;

const MAX_SAMPLE_SPREAD: f64 = 10.0;
const MAX_PRIMARY_SATURATION_NUMERATOR: usize = 9;
const MAX_PRIMARY_SATURATION_DENOMINATOR: usize = 10;
const PRIMARY_SATURATION_REJECTION_FRACTION: f64 = 0.90;

const HELP: &str = "\
RustHouse / ClickHouse Local black-box parity benchmark

USAGE:
    clickhouse-parity-bench [OPTIONS]

OPTIONS:
    --mode <quick|default>  Benchmark size (default: default)
    --quick                 Alias for --mode quick
    --seed <U64>            Deterministic root seed (default: 20260729)
    --clickhouse <PATH>     ClickHouse 26.7.1 binary
    --rusthouse <PATH>      Prebuilt rusthouse CLI (default: sibling binary)
    --details <PATH>        Write detailed JSON without changing stdout
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
    seed: u64,
    dataset_seed: u64,
    row_count: usize,
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
    accepted: bool,
    score: f64,
    measured_score: f64,
    summary: String,
    evidence: Vec<String>,
    suggestions: Vec<String>,
}

fn main() -> ExitCode {
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
        accepted: false,
        score: 0.0,
        measured_score: 0.0,
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
    let benchmark_seeds = config.mode.benchmark_seeds(config.seed);
    let paths = EnginePaths {
        rusthouse: config.rusthouse.clone(),
        clickhouse: config.clickhouse.clone(),
    };
    let identity = paths.validate()?;
    let mut cases = Vec::new();
    let mut correctness_checks = 0_usize;

    for (seed_index, seed) in benchmark_seeds.iter().copied().enumerate() {
        for (row_count_index, row_count) in settings.row_counts.iter().copied().enumerate() {
            let dataset_seed = seed ^ (row_count as u64).wrapping_mul(0xd6e8_feb8_6659_fd93);
            let dataset = Dataset::generate(dataset_seed, row_count);
            let setup_sql = dataset.setup_sql();

            for (workload_index, workload) in workloads(row_count).into_iter().enumerate() {
                eprintln!(
                    "benchmarking {} with seed {} at {} rows ({}x amplification, {} warmups, {} primary samples, {} end-to-end samples)",
                    workload.name,
                    seed,
                    row_count,
                    settings.query_amplification,
                    settings.warmups,
                    settings.samples,
                    settings.end_to_end_samples
                );

                let correctness_order =
                    (seed_index + row_count_index + workload_index).is_multiple_of(2);
                let (rusthouse_output, clickhouse_output) =
                    execute_correctness_pair(&paths, &setup_sql, &workload.sql, correctness_order)?;
                let mut correctness_gate = CorrectnessGate::default();
                correctness_gate.verify(
                    &workload.columns,
                    &rusthouse_output,
                    &clickhouse_output,
                ).map_err(|error| {
                    format!(
                        "correctness gate failed for '{}' with seed {seed} at {row_count} rows: {error}",
                        workload.name,
                    )
                })?;
                correctness_checks += 1;

                let mut primary = TimingSeries::default();
                let primary_iterations = settings.warmups + settings.samples;
                for iteration in 0..primary_iterations {
                    let rusthouse_first =
                        (seed_index + row_count_index + workload_index + iteration + 1)
                            .is_multiple_of(2);
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
                    let rusthouse_first = (seed_index
                        + row_count_index
                        + workload_index
                        + iteration
                        + primary_iterations)
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
                    seed,
                    dataset_seed,
                    row_count,
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
    }

    let primary_score = score_cases(&cases, &benchmark_seeds, &settings.row_counts, |case| {
        case.primary_ratio
    })?;
    if config.mode == config::Mode::Default {
        ensure_primary_headroom(&primary_score, cases.len())?;
    }
    let end_to_end_score = score_cases(&cases, &benchmark_seeds, &settings.row_counts, |case| {
        case.end_to_end_ratio
    })?;

    if let Some(path) = &config.details {
        let details = details_json(
            &config,
            &identity,
            &cases,
            primary_score,
            end_to_end_score,
            correctness_checks,
        );
        fs::write(path, details)
            .map_err(|error| format!("could not write details to '{}': {error}", path.display()))?;
    }

    let mut evidence = vec![
        format!(
            "{} separate correctness pairs passed across {} cases, {} seeds, and {} row counts",
            correctness_checks,
            cases.len(),
            benchmark_seeds.len(),
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
            "mode={}, root_seed={}, seed_panel={:?}, warmups={}, primary_samples={}, end_to_end_samples={}; ClickHouse SHA-256={}",
            config.mode.name(),
            config.seed,
            benchmark_seeds,
            settings.warmups,
            settings.samples,
            settings.end_to_end_samples,
            identity.sha256
        ),
        format!("ClickHouse identity: {}", identity.version_output),
        format!(
            "limitation: amplification measures repeated warm in-process work, retains 1/{} of startup/setup, and does not model concurrency, durable storage, or network access",
            settings.query_amplification
        ),
    ];
    evidence.extend(cases.iter().map(|case| {
        format!(
            "{} / seed {} / {} rows: primary/query RustHouse {:.3} ms, ClickHouse {:.3} ms, ratio {:.3}; end-to-end RustHouse {:.3} ms, ClickHouse {:.3} ms, ratio {:.3}",
            case.workload,
            case.seed,
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
            "Use --mode default for the decision-grade three-seed 1k/10k/50k/250k-row suite."
                .to_owned(),
            "Treat the quick single-seed score as a smoke test, not an acceptance result."
                .to_owned(),
        ]
    } else {
        vec![
            "Compare detailed case medians across all required seeds and scales before accepting an optimization."
                .to_owned(),
            "Treat regressions in correctness as score zero, regardless of timing improvements."
                .to_owned(),
        ]
    };

    let summary = if config.mode == config::Mode::Quick {
        format!(
            "Diagnostic quick run measured primary score {:.2} and startup-inclusive end-to-end score {:.2} over {} correctness-gated cases; quick mode is not accepted.",
            primary_score.score,
            end_to_end_score.score,
            cases.len()
        )
    } else {
        format!(
            "RustHouse primary sustained-work score {:.2}; startup-inclusive end-to-end score {:.2}; ClickHouse parity=100 over {} correctness-gated cases.",
            primary_score.score,
            end_to_end_score.score,
            cases.len()
        )
    };

    Ok(Report::benchmark(
        config.mode,
        primary_score.score,
        summary,
        evidence,
        suggestions,
    ))
}

fn score_cases(
    cases: &[CaseResult],
    seeds: &[u64],
    scales: &[usize],
    ratio: impl Fn(&CaseResult) -> f64,
) -> Result<ScoreBreakdown, String> {
    let expected_cases = workloads(1)
        .into_iter()
        .map(|workload| ExpectedCase {
            name: workload.name,
            family: workload.family.name(),
        })
        .collect::<Vec<_>>();
    let observations = cases
        .iter()
        .map(|case| RatioObservation {
            case: case.workload,
            family: case.family,
            seed: case.seed,
            scale: case.row_count,
            ratio: ratio(case),
        })
        .collect::<Vec<_>>();
    parity_score(
        &observations,
        ScorePanel {
            seeds,
            scales,
            cases: &expected_cases,
        },
    )
}

fn ensure_primary_headroom(score: &ScoreBreakdown, case_count: usize) -> Result<(), String> {
    if case_count == 0 {
        return Err("primary timing cannot be accepted without cases".to_owned());
    }
    if score
        .saturated_cases
        .saturating_mul(MAX_PRIMARY_SATURATION_DENOMINATOR)
        >= case_count.saturating_mul(MAX_PRIMARY_SATURATION_NUMERATOR)
    {
        return Err(format!(
            "primary timing saturated: {}/{} cases reached the parity cap; default acceptance requires fewer than 90% of cases at parity",
            score.saturated_cases, case_count
        ));
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

fn details_json(
    config: &Config,
    identity: &ClickHouseIdentity,
    cases: &[CaseResult],
    primary_score: ScoreBreakdown,
    end_to_end_score: ScoreBreakdown,
    correctness_checks: usize,
) -> String {
    let settings = config.mode.settings();
    let benchmark_seeds = config.mode.benchmark_seeds(config.seed);
    let accepted = config.mode == config::Mode::Default;
    let published_score = if accepted { primary_score.score } else { 0.0 };
    let mut output = String::new();
    write!(
        output,
        "{{\"schema_version\":4,\"accepted\":{accepted},\"score\":{:.6},\"primary_score\":{:.6},\"end_to_end_score\":{:.6},\"primary_saturated_cases\":{},\"end_to_end_saturated_cases\":{},\"primary_saturation_rejection_fraction\":{PRIMARY_SATURATION_REJECTION_FRACTION:.2},\"mode\":{},\"root_seed\":{},\"warmups\":{},\"primary_samples\":{},\"end_to_end_samples\":{},\"seeds\":[",
        published_score,
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
    for (index, seed) in benchmark_seeds.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        write!(output, "{seed}").expect("writing to String cannot fail");
    }
    output.push_str("],\"row_counts\":[");
    for (index, row_count) in settings.row_counts.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        write!(output, "{row_count}").expect("writing to String cannot fail");
    }
    write!(
        output,
        "],\"aggregation\":\"equal_log_weight_by_workload_within_family_then_seed_then_scale_then_family\",\"timing_method\":{{\"name\":\"in_process_query_amplification\",\"calibration\":\"fixed_shared_repetitions\",\"query_amplification\":{},\"startup_subtraction\":false,\"correctness_runs_separate\":true,\"max_sample_spread\":{MAX_SAMPLE_SPREAD:.1}}},\"correctness_checks\":{correctness_checks},\"rusthouse_path\":{},\"clickhouse_path\":{},\"clickhouse_version\":{},\"clickhouse_sha256\":{},\"limitations\":[{},{}],\"cases\":[",
        settings.query_amplification,
        json_string(&config.rusthouse.display().to_string()),
        json_string(&config.clickhouse.display().to_string()),
        json_string(&identity.version_output),
        json_string(&identity.sha256),
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
            "{{\"workload\":{},\"family\":{},\"seed\":{},\"dataset_seed\":{},\"row_count\":{},\"query_amplification\":{},\"primary\":{{\"rusthouse_batch_median_ms\":{:.6},\"clickhouse_batch_median_ms\":{:.6},\"rusthouse_per_query_median_ms\":{:.6},\"clickhouse_per_query_median_ms\":{:.6},\"clickhouse_rusthouse_ratio\":{:.9},\"rusthouse_batch_samples_ms\":",
            json_string(case.workload),
            json_string(case.family),
            case.seed,
            case.dataset_seed,
            case.row_count,
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
    fn benchmark(
        mode: config::Mode,
        measured_score: f64,
        summary: String,
        evidence: Vec<String>,
        suggestions: Vec<String>,
    ) -> Self {
        let accepted = mode == config::Mode::Default;
        Self {
            accepted,
            score: if accepted { measured_score } else { 0.0 },
            measured_score,
            summary,
            evidence,
            suggestions,
        }
    }

    fn to_json(&self) -> String {
        let mut output = format!(
            "{{\"accepted\":{},\"score\":{:.6},\"measured_score\":{:.6},\"summary\":{},\"evidence\":[",
            self.accepted,
            self.score,
            self.measured_score,
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
    fn default_saturation_gate_can_accept_the_complete_calibrated_panel() {
        let acceptable = ScoreBreakdown {
            score: 99.0,
            saturated_cases: 86,
        };
        ensure_primary_headroom(&acceptable, 96)
            .expect("at least ten percent uncapped cases retains useful headroom");

        let rejected = ScoreBreakdown {
            score: 100.0,
            saturated_cases: 87,
        };
        let error = ensure_primary_headroom(&rejected, 96).expect_err("90% saturation must fail");
        assert!(error.contains("fewer than 90%"));

        let exact_fraction = ScoreBreakdown {
            score: 100.0,
            saturated_cases: 90,
        };
        ensure_primary_headroom(&exact_fraction, 100).expect_err("exactly 90% must fail");
    }

    #[test]
    fn quick_report_is_structurally_and_numerically_non_acceptable() {
        let report = Report::benchmark(
            config::Mode::Quick,
            100.0,
            "diagnostic".to_owned(),
            vec![],
            vec![],
        );
        assert!(!report.accepted);
        assert_eq!(report.score, 0.0);
        assert_eq!(report.measured_score, 100.0);
        assert!(
            report.to_json().starts_with(
                "{\"accepted\":false,\"score\":0.000000,\"measured_score\":100.000000"
            )
        );
    }

    #[test]
    fn burner_report_is_one_compact_object_with_required_fields() {
        let report = Report {
            accepted: true,
            score: 10.0,
            measured_score: 10.0,
            summary: "summary".to_owned(),
            evidence: vec!["evidence".to_owned()],
            suggestions: vec!["suggestion".to_owned()],
        }
        .to_json();

        assert_eq!(
            report,
            "{\"accepted\":true,\"score\":10.000000,\"measured_score\":10.000000,\"summary\":\"summary\",\"evidence\":[\"evidence\"],\"suggestions\":[\"suggestion\"]}"
        );
        assert!(!report.contains('\n'));
    }
}
