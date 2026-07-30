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
use normalize::{ColumnType, compare_outputs, compare_outputs_named};
use process::{
    BenchmarkIdentity, Engine, EnginePaths, RustHouseIdentity, RustHouseSnapshot, TimedBatch,
    TimedOutput, validate_rusthouse,
};
use score::{
    RatioObservation, ScoreBreakdown, UncappedRatioBreakdown, median, parity_score, uncapped_ratio,
};
use workload::workloads;

const MAX_SAMPLE_SPREAD: f64 = 10.0;
const MAX_CASE_REGRESSION: f64 = 0.20;
const MAX_FAMILY_REGRESSION: f64 = 0.10;

const HELP: &str = "\
RustHouse / ClickHouse Local black-box parity benchmark

USAGE:
    clickhouse-parity-bench [OPTIONS]

OPTIONS:
    --mode <quick|default>  Benchmark size (default: default)
    --quick                 Alias for --mode quick
    --seed <U64>            Deterministic runtime seed (default: 20260729)
    --clickhouse <PATH>     ClickHouse 26.7.1 binary
    --rusthouse <PATH>      Candidate RustHouse CLI (default: sibling binary)
    --baseline <PATH>       Enable regression gates against this RustHouse CLI
    --details <PATH>        Write detailed JSON without changing stdout
    -h, --help              Print this help

RUSTHOUSE_CLICKHOUSE_BIN supplies --clickhouse when the flag is absent.
RUSTHOUSE_BIN supplies --rusthouse when the flag is absent.
RUSTHOUSE_BASELINE_BIN supplies --baseline when the flag is absent.
Baseline mode requires --details to retain raw samples and binary hashes.
Build release binaries before benchmarking; compilation is never timed.
";

#[derive(Debug, Default)]
struct TimingSeries {
    rusthouse_batch_ms: Vec<f64>,
    clickhouse_batch_ms: Vec<f64>,
    rusthouse_per_query_ms: Vec<f64>,
    clickhouse_per_query_ms: Vec<f64>,
}

#[derive(Debug, Default)]
struct RegressionTimingSeries {
    candidate_batch_ms: Vec<f64>,
    baseline_batch_ms: Vec<f64>,
    candidate_per_query_ms: Vec<f64>,
    baseline_per_query_ms: Vec<f64>,
    candidate_first: Vec<bool>,
}

#[derive(Debug)]
struct RegressionCaseResult {
    primary: RegressionTimingSeries,
    candidate_primary_batch_median_ms: f64,
    baseline_primary_batch_median_ms: f64,
    candidate_primary_median_ms: f64,
    baseline_primary_median_ms: f64,
    primary_ratio: f64,
    end_to_end: RegressionTimingSeries,
    candidate_end_to_end_median_ms: f64,
    baseline_end_to_end_median_ms: f64,
    end_to_end_ratio: f64,
}

#[derive(Debug)]
struct CaseResult {
    workload: &'static str,
    family: &'static str,
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
    regression: Option<RegressionCaseResult>,
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

struct RunOutcome {
    report: Report,
    regression_failed: bool,
}

struct RegressionAnalysis {
    primary: UncappedRatioBreakdown<'static>,
    end_to_end: UncappedRatioBreakdown<'static>,
    violations: Vec<String>,
}

struct RegressionDetails<'a> {
    correctness_checks: usize,
    baseline_identity: Option<&'a RustHouseIdentity>,
    analysis: Option<&'a RegressionAnalysis>,
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
        env::var("RUSTHOUSE_BASELINE_BIN").ok(),
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
        Ok(outcome) => {
            println!("{}", outcome.report.to_json());
            if outcome.regression_failed {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
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

fn run(config: Config) -> Result<RunOutcome, String> {
    let settings = config.mode.settings();
    let candidate_snapshot = RustHouseSnapshot::create(&config.rusthouse, "candidate")?;
    let paths = EnginePaths {
        rusthouse: candidate_snapshot.path().to_owned(),
        clickhouse: config.clickhouse.clone(),
    };
    let identity = paths.validate()?;
    let mut cases = Vec::new();
    let mut correctness_checks = 0_usize;
    let mut regression_correctness_checks = 0_usize;

    for (row_count_index, row_count) in settings.row_counts.iter().copied().enumerate() {
        let dataset_seed = config.seed ^ (row_count as u64).wrapping_mul(0xd6e8_feb8_6659_fd93);
        let dataset = Dataset::generate(dataset_seed, row_count);
        let setup_sql = dataset.setup_sql();

        for (workload_index, workload) in workloads(row_count).into_iter().enumerate() {
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
                regression: None,
            });
        }
    }

    let primary_score = score_cases(&cases, |case| case.primary_ratio)?;
    if config.mode == config::Mode::Default {
        ensure_primary_headroom(&primary_score, cases.len())?;
    }
    let end_to_end_score = score_cases(&cases, |case| case.end_to_end_ratio)?;
    let mut baseline_identity = None;
    if let Some(baseline_source) = &config.baseline {
        eprintln!(
            "preparing baseline snapshot after ClickHouse scoring completed; starting isolated candidate/baseline regression suite"
        );
        let baseline_snapshot = RustHouseSnapshot::create(baseline_source, "baseline")?;
        let identity = validate_rusthouse(baseline_snapshot.path())
            .map_err(|error| format!("baseline validation failed: {error}"))?;
        let baseline_paths = EnginePaths {
            rusthouse: baseline_snapshot.path().to_owned(),
            clickhouse: config.clickhouse.clone(),
        };
        let mut case_index = 0_usize;
        for (row_count_index, row_count) in settings.row_counts.iter().copied().enumerate() {
            let dataset_seed = config.seed ^ (row_count as u64).wrapping_mul(0xd6e8_feb8_6659_fd93);
            let dataset = Dataset::generate(dataset_seed, row_count);
            let setup_sql = dataset.setup_sql();

            for (workload_index, workload) in workloads(row_count).into_iter().enumerate() {
                let candidate_first = (row_count_index + workload_index).is_multiple_of(2);
                let (candidate_output, baseline_output) = execute_regression_correctness_pair(
                    &paths,
                    &baseline_paths,
                    &setup_sql,
                    &workload.sql,
                    candidate_first,
                )?;
                compare_outputs_named(
                    &candidate_output.stdout,
                    "candidate RustHouse",
                    &baseline_output.stdout,
                    "baseline RustHouse",
                    &workload.columns,
                )
                .map_err(|error| {
                    format!(
                        "candidate/baseline correctness gate failed for '{}' at {row_count} rows: {error}",
                        workload.name
                    )
                })?;
                regression_correctness_checks += 1;
                let gate = CorrectnessGate { passed: true };
                let regression = measure_regression_case(
                    &paths,
                    &baseline_paths,
                    &gate,
                    &setup_sql,
                    &workload.sql,
                    workload.name,
                    row_count,
                    row_count_index + workload_index,
                    settings.warmups,
                    settings.samples,
                    settings.query_amplification,
                    settings.end_to_end_samples,
                )?;
                let case = cases.get_mut(case_index).ok_or_else(|| {
                    "candidate/baseline suite produced more cases than ClickHouse suite".to_owned()
                })?;
                if case.workload != workload.name || case.row_count != row_count {
                    return Err(
                        "candidate/baseline suite case order diverged from ClickHouse suite"
                            .to_owned(),
                    );
                }
                case.regression = Some(regression);
                case_index += 1;
            }
        }
        if case_index != cases.len() {
            return Err(
                "candidate/baseline suite produced fewer cases than ClickHouse suite".to_owned(),
            );
        }
        baseline_snapshot.verify_unchanged(&identity, "baseline")?;
        baseline_identity = Some(identity);
    }
    candidate_snapshot.verify_unchanged(&identity.rusthouse, "candidate")?;
    let regression_analysis = if config.baseline.is_some() {
        let primary = regression_ratios(&cases, |regression| regression.primary_ratio)?;
        let end_to_end = regression_ratios(&cases, |regression| regression.end_to_end_ratio)?;
        let violations = regression_violations(&cases, &primary);
        Some(RegressionAnalysis {
            primary,
            end_to_end,
            violations,
        })
    } else {
        None
    };

    if let Some(path) = &config.details {
        let details = details_json(
            &config,
            &identity,
            &cases,
            primary_score,
            end_to_end_score,
            correctness_checks,
            RegressionDetails {
                correctness_checks: regression_correctness_checks,
                baseline_identity: baseline_identity.as_ref(),
                analysis: regression_analysis.as_ref(),
            },
        );
        fs::write(path, details)
            .map_err(|error| format!("could not write details to '{}': {error}", path.display()))?;
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
            "mode={}, seed={}, warmups={}, primary_samples={}, end_to_end_samples={}; ClickHouse SHA-256={}",
            config.mode.name(),
            config.seed,
            settings.warmups,
            settings.samples,
            settings.end_to_end_samples,
            identity.clickhouse.sha256
        ),
        format!(
            "ClickHouse identity: {}",
            identity.clickhouse.version_output
        ),
        format!(
            "candidate RustHouse immutable snapshot verified after timing; SHA-256={}",
            identity.rusthouse.sha256
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
    if let (Some(baseline_identity), Some(regression)) = (&baseline_identity, &regression_analysis)
    {
        evidence.push(format!(
            "candidate/baseline primary ratio {:.4} and end-to-end ratio {:.4} (uncapped baseline/candidate); deferred baseline snapshot verified after timing; SHA-256={}",
            regression.primary.ratio,
            regression.end_to_end.ratio,
            baseline_identity.sha256
        ));
        evidence.extend(regression.primary.families.iter().map(|family| {
            format!(
                "candidate/baseline family {}: primary uncapped ratio {:.4}",
                family.family, family.ratio
            )
        }));
        evidence.extend(cases.iter().filter_map(|case| {
            case.regression.as_ref().map(|case_regression| {
                format!(
                    "candidate/baseline {} / {} rows: primary uncapped ratio {:.4}; end-to-end uncapped ratio {:.4}",
                    case.workload,
                    case.row_count,
                    case_regression.primary_ratio,
                    case_regression.end_to_end_ratio
                )
            })
        }));
        evidence.extend(regression.violations.iter().cloned());
    }
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

    let regression_failed = regression_analysis
        .as_ref()
        .is_some_and(|analysis| !analysis.violations.is_empty());
    let regression_status = match &regression_analysis {
        Some(analysis) if analysis.violations.is_empty() => {
            format!(
                " Candidate/baseline regression gates passed at {:.4}.",
                analysis.primary.ratio
            )
        }
        Some(analysis) => format!(
            " Candidate/baseline regression gates failed with {} violation(s); ClickHouse score is unchanged.",
            analysis.violations.len()
        ),
        None => String::new(),
    };

    Ok(RunOutcome {
        report: Report {
            score: primary_score.score,
            summary: format!(
                "RustHouse primary sustained-work score {:.2}; startup-inclusive end-to-end score {:.2}; ClickHouse parity=100 over {} correctness-gated cases.{}",
                primary_score.score,
                end_to_end_score.score,
                cases.len(),
                regression_status
            ),
            evidence,
            suggestions,
        },
        regression_failed,
    })
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

#[allow(clippy::too_many_arguments)]
fn measure_regression_case(
    candidate_paths: &EnginePaths,
    baseline_paths: &EnginePaths,
    gate: &CorrectnessGate,
    setup_sql: &str,
    query_sql: &str,
    workload: &str,
    row_count: usize,
    order_offset: usize,
    warmups: usize,
    samples: usize,
    query_amplification: usize,
    end_to_end_samples: usize,
) -> Result<RegressionCaseResult, String> {
    let mut primary = RegressionTimingSeries::default();
    for iteration in 0..warmups + samples {
        let candidate_first = candidate_runs_first(order_offset, iteration);
        let (candidate, baseline) = execute_regression_timed_pair(
            candidate_paths,
            baseline_paths,
            setup_sql,
            query_sql,
            query_amplification,
            candidate_first,
        )?;
        accept_regression_timed_pair(
            gate,
            &candidate,
            &baseline,
            query_amplification,
            iteration >= warmups,
            candidate_first,
            &mut primary,
        )?;
    }

    let mut end_to_end = RegressionTimingSeries::default();
    for iteration in 0..end_to_end_samples {
        let candidate_first = candidate_runs_first(order_offset, warmups + samples + iteration);
        let (candidate, baseline) = execute_regression_timed_pair(
            candidate_paths,
            baseline_paths,
            setup_sql,
            query_sql,
            1,
            candidate_first,
        )?;
        accept_regression_timed_pair(
            gate,
            &candidate,
            &baseline,
            1,
            true,
            candidate_first,
            &mut end_to_end,
        )?;
    }

    let candidate_primary_batch_median_ms = stable_median(
        &primary.candidate_batch_ms,
        "candidate RustHouse amplified batch",
        workload,
        row_count,
    )?;
    let baseline_primary_batch_median_ms = stable_median(
        &primary.baseline_batch_ms,
        "baseline RustHouse amplified batch",
        workload,
        row_count,
    )?;
    let candidate_primary_median_ms = stable_median(
        &primary.candidate_per_query_ms,
        "candidate RustHouse amortized query",
        workload,
        row_count,
    )?;
    let baseline_primary_median_ms = stable_median(
        &primary.baseline_per_query_ms,
        "baseline RustHouse amortized query",
        workload,
        row_count,
    )?;
    let candidate_end_to_end_median_ms = stable_median(
        &end_to_end.candidate_batch_ms,
        "candidate RustHouse end-to-end",
        workload,
        row_count,
    )?;
    let baseline_end_to_end_median_ms = stable_median(
        &end_to_end.baseline_batch_ms,
        "baseline RustHouse end-to-end",
        workload,
        row_count,
    )?;
    let primary_ratio = baseline_primary_median_ms / candidate_primary_median_ms;
    let end_to_end_ratio = baseline_end_to_end_median_ms / candidate_end_to_end_median_ms;

    eprintln!(
        "  candidate/baseline: primary uncapped ratio {:.3}; end-to-end uncapped ratio {:.3}",
        primary_ratio, end_to_end_ratio
    );
    Ok(RegressionCaseResult {
        primary,
        candidate_primary_batch_median_ms,
        baseline_primary_batch_median_ms,
        candidate_primary_median_ms,
        baseline_primary_median_ms,
        primary_ratio,
        end_to_end,
        candidate_end_to_end_median_ms,
        baseline_end_to_end_median_ms,
        end_to_end_ratio,
    })
}

fn candidate_runs_first(order_offset: usize, iteration: usize) -> bool {
    (order_offset + iteration).is_multiple_of(2)
}

fn regression_ratios(
    cases: &[CaseResult],
    ratio: impl Fn(&RegressionCaseResult) -> f64,
) -> Result<UncappedRatioBreakdown<'static>, String> {
    let observations = cases
        .iter()
        .map(|case| {
            let regression = case.regression.as_ref().ok_or_else(|| {
                format!(
                    "missing candidate/baseline timing for '{}' at {} rows",
                    case.workload, case.row_count
                )
            })?;
            Ok(RatioObservation {
                family: case.family,
                scale: case.row_count,
                ratio: ratio(regression),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    uncapped_ratio(&observations)
}

fn regression_violations(
    cases: &[CaseResult],
    primary: &UncappedRatioBreakdown<'_>,
) -> Vec<String> {
    let mut violations = cases
        .iter()
        .filter_map(|case| {
            let ratio = case.regression.as_ref()?.primary_ratio;
            regression_exceeds_limit(ratio, MAX_CASE_REGRESSION).then(|| {
                format!(
                    "regression gate: '{}' at {} rows is {:.2}% slower than baseline (uncapped ratio {:.4}; limit {:.0}%)",
                    case.workload,
                    case.row_count,
                    (1.0 / ratio - 1.0) * 100.0,
                    ratio,
                    MAX_CASE_REGRESSION * 100.0
                )
            })
        })
        .collect::<Vec<_>>();
    violations.extend(
        primary
            .families
            .iter()
            .filter(|family| {
                regression_exceeds_limit(family.ratio, MAX_FAMILY_REGRESSION)
            })
            .map(|family| {
            format!(
                "regression gate: family '{}' is {:.2}% slower than baseline (uncapped ratio {:.4}; limit {:.0}%)",
                family.family,
                (1.0 / family.ratio - 1.0) * 100.0,
                family.ratio,
                MAX_FAMILY_REGRESSION * 100.0
            )
            }),
    );
    violations
}

fn regression_exceeds_limit(baseline_candidate_ratio: f64, maximum_regression: f64) -> bool {
    baseline_candidate_ratio < 1.0 / (1.0 + maximum_regression)
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

fn execute_regression_correctness_pair(
    candidate_paths: &EnginePaths,
    baseline_paths: &EnginePaths,
    setup_sql: &str,
    query_sql: &str,
    candidate_first: bool,
) -> Result<(TimedOutput, TimedOutput), String> {
    let execute_candidate = || {
        candidate_paths
            .execute_correctness(Engine::RustHouse, setup_sql, query_sql)
            .map_err(|error| format!("candidate correctness execution failed: {error}"))
    };
    let execute_baseline = || {
        baseline_paths
            .execute_correctness(Engine::RustHouse, setup_sql, query_sql)
            .map_err(|error| format!("baseline correctness execution failed: {error}"))
    };
    if candidate_first {
        Ok((execute_candidate()?, execute_baseline()?))
    } else {
        let baseline = execute_baseline()?;
        let candidate = execute_candidate()?;
        Ok((candidate, baseline))
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

fn execute_regression_timed_pair(
    candidate_paths: &EnginePaths,
    baseline_paths: &EnginePaths,
    setup_sql: &str,
    query_sql: &str,
    query_repetitions: usize,
    candidate_first: bool,
) -> Result<(TimedBatch, TimedBatch), String> {
    let execute_candidate = || {
        candidate_paths
            .execute_timed(Engine::RustHouse, setup_sql, query_sql, query_repetitions)
            .map_err(|error| format!("candidate timing failed: {error}"))
    };
    let execute_baseline = || {
        baseline_paths
            .execute_timed(Engine::RustHouse, setup_sql, query_sql, query_repetitions)
            .map_err(|error| format!("baseline timing failed: {error}"))
    };
    if candidate_first {
        Ok((execute_candidate()?, execute_baseline()?))
    } else {
        let baseline = execute_baseline()?;
        let candidate = execute_candidate()?;
        Ok((candidate, baseline))
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

fn accept_regression_timed_pair(
    gate: &CorrectnessGate,
    candidate: &TimedBatch,
    baseline: &TimedBatch,
    expected_repetitions: usize,
    record: bool,
    candidate_first: bool,
    samples: &mut RegressionTimingSeries,
) -> Result<(), String> {
    if !gate.passed {
        return Err(
            "candidate/baseline timed batch was not preceded by a passing correctness run"
                .to_owned(),
        );
    }
    if candidate.query_repetitions != baseline.query_repetitions
        || candidate.query_repetitions != expected_repetitions
    {
        return Err(format!(
            "candidate/baseline amplification mismatch: expected {expected_repetitions}, candidate used {}, baseline used {}",
            candidate.query_repetitions, baseline.query_repetitions
        ));
    }

    let candidate_batch_ms = candidate.elapsed.as_secs_f64() * 1_000.0;
    let baseline_batch_ms = baseline.elapsed.as_secs_f64() * 1_000.0;
    let candidate_per_query_ms = per_query_millis(candidate_batch_ms, candidate.query_repetitions)?;
    let baseline_per_query_ms = per_query_millis(baseline_batch_ms, baseline.query_repetitions)?;
    if record {
        samples.candidate_batch_ms.push(candidate_batch_ms);
        samples.baseline_batch_ms.push(baseline_batch_ms);
        samples.candidate_per_query_ms.push(candidate_per_query_ms);
        samples.baseline_per_query_ms.push(baseline_per_query_ms);
        samples.candidate_first.push(candidate_first);
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
    identity: &BenchmarkIdentity,
    cases: &[CaseResult],
    primary_score: ScoreBreakdown,
    end_to_end_score: ScoreBreakdown,
    correctness_checks: usize,
    regression_details: RegressionDetails<'_>,
) -> String {
    let settings = config.mode.settings();
    let mut output = String::new();
    write!(
        output,
        "{{\"schema_version\":3,\"score\":{:.6},\"primary_score\":{:.6},\"end_to_end_score\":{:.6},\"primary_saturated_cases\":{},\"end_to_end_saturated_cases\":{},\"mode\":{},\"seed\":{},\"warmups\":{},\"primary_samples\":{},\"end_to_end_samples\":{},\"row_counts\":[",
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
        "],\"timing_method\":{{\"name\":\"in_process_query_amplification\",\"calibration\":\"fixed_shared_repetitions\",\"query_amplification\":{},\"startup_subtraction\":false,\"correctness_runs_separate\":true,\"max_sample_spread\":{MAX_SAMPLE_SPREAD:.1}}},\"correctness_checks\":{correctness_checks},\"rusthouse_path\":{},\"rusthouse_sha256\":{},\"clickhouse_path\":{},\"clickhouse_version\":{},\"clickhouse_sha256\":{},\"limitations\":[{},{}],",
        settings.query_amplification,
        json_string(&config.rusthouse.display().to_string()),
        json_string(&identity.rusthouse.sha256),
        json_string(&config.clickhouse.display().to_string()),
        json_string(&identity.clickhouse.version_output),
        json_string(&identity.clickhouse.sha256),
        json_string("amplification measures repeated warm in-process work and retains one divided by the amplification factor of startup and setup"),
        json_string("synthetic single-process data does not model concurrency, durable storage, networking, joins, nullability, or production compression")
    )
    .expect("writing to String cannot fail");
    write_regression_metadata(
        &mut output,
        config,
        regression_details.correctness_checks,
        &identity.rusthouse,
        regression_details.baseline_identity,
        regression_details.analysis,
    );
    output.push_str("\"cases\":[");

    for (index, case) in cases.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        write!(
            output,
            "{{\"workload\":{},\"family\":{},\"row_count\":{},\"query_amplification\":{},\"primary\":{{\"rusthouse_batch_median_ms\":{:.6},\"clickhouse_batch_median_ms\":{:.6},\"rusthouse_per_query_median_ms\":{:.6},\"clickhouse_per_query_median_ms\":{:.6},\"clickhouse_rusthouse_ratio\":{:.9},\"rusthouse_batch_samples_ms\":",
            json_string(case.workload),
            json_string(case.family),
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
        output.push('}');
        if let Some(regression) = &case.regression {
            output.push_str(",\"candidate_baseline\":{\"primary\":{");
            write!(
                output,
                "\"candidate_batch_median_ms\":{:.6},\"baseline_batch_median_ms\":{:.6},\"candidate_per_query_median_ms\":{:.6},\"baseline_per_query_median_ms\":{:.6},\"baseline_candidate_ratio\":{:.9},\"candidate_batch_samples_ms\":",
                regression.candidate_primary_batch_median_ms,
                regression.baseline_primary_batch_median_ms,
                regression.candidate_primary_median_ms,
                regression.baseline_primary_median_ms,
                regression.primary_ratio
            )
            .expect("writing to String cannot fail");
            write_number_array(&mut output, &regression.primary.candidate_batch_ms);
            output.push_str(",\"baseline_batch_samples_ms\":");
            write_number_array(&mut output, &regression.primary.baseline_batch_ms);
            output.push_str(",\"candidate_per_query_samples_ms\":");
            write_number_array(&mut output, &regression.primary.candidate_per_query_ms);
            output.push_str(",\"baseline_per_query_samples_ms\":");
            write_number_array(&mut output, &regression.primary.baseline_per_query_ms);
            output.push_str(",\"candidate_first\":");
            write_bool_array(&mut output, &regression.primary.candidate_first);
            write!(
                output,
                "}},\"end_to_end\":{{\"candidate_median_ms\":{:.6},\"baseline_median_ms\":{:.6},\"baseline_candidate_ratio\":{:.9},\"candidate_samples_ms\":",
                regression.candidate_end_to_end_median_ms,
                regression.baseline_end_to_end_median_ms,
                regression.end_to_end_ratio
            )
            .expect("writing to String cannot fail");
            write_number_array(&mut output, &regression.end_to_end.candidate_batch_ms);
            output.push_str(",\"baseline_samples_ms\":");
            write_number_array(&mut output, &regression.end_to_end.baseline_batch_ms);
            output.push_str(",\"candidate_first\":");
            write_bool_array(&mut output, &regression.end_to_end.candidate_first);
            output.push_str("}}");
        }
        output.push('}');
    }
    output.push_str("]}\n");
    output
}

fn write_regression_metadata(
    output: &mut String,
    config: &Config,
    correctness_checks: usize,
    candidate_identity: &RustHouseIdentity,
    baseline_identity: Option<&RustHouseIdentity>,
    regression: Option<&RegressionAnalysis>,
) {
    let (Some(baseline_path), Some(baseline_identity), Some(regression)) =
        (&config.baseline, baseline_identity, regression)
    else {
        output.push_str("\"candidate_baseline\":null,");
        return;
    };
    write!(
        output,
        "\"candidate_baseline\":{{\"candidate_path\":{},\"candidate_sha256\":{},\"baseline_path\":{},\"baseline_sha256\":{},\"correctness_checks\":{},\"ratio_definition\":{},\"counterbalance\":{},\"binary_isolation\":{},\"gates\":{{\"metric\":\"primary_sustained_work\",\"max_case_regression_fraction\":{MAX_CASE_REGRESSION:.6},\"max_family_regression_fraction\":{MAX_FAMILY_REGRESSION:.6}}},\"passed\":{},\"primary_overall_ratio\":{:.9},\"end_to_end_overall_ratio\":{:.9},\"primary_family_ratios\":",
        json_string(&config.rusthouse.display().to_string()),
        json_string(&candidate_identity.sha256),
        json_string(&baseline_path.display().to_string()),
        json_string(&baseline_identity.sha256),
        correctness_checks,
        json_string("uncapped baseline median divided by candidate median; values below one are regressions"),
        json_string("candidate-first and baseline-first launch order alternates per case and is retained per sample"),
        json_string("all RustHouse samples execute sealed snapshots whose hashes are verified after timing; baseline snapshot preparation begins after ClickHouse scoring"),
        regression.violations.is_empty(),
        regression.primary.ratio,
        regression.end_to_end.ratio
    )
    .expect("writing to String cannot fail");
    write_family_ratios(output, &regression.primary);
    output.push_str(",\"end_to_end_family_ratios\":");
    write_family_ratios(output, &regression.end_to_end);
    output.push_str(",\"violations\":[");
    write_string_array(output, &regression.violations);
    output.push_str("]},");
}

fn write_family_ratios(output: &mut String, ratios: &UncappedRatioBreakdown<'_>) {
    output.push('[');
    for (index, family) in ratios.families.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        write!(
            output,
            "{{\"family\":{},\"baseline_candidate_ratio\":{:.9}}}",
            json_string(family.family),
            family.ratio
        )
        .expect("writing to String cannot fail");
    }
    output.push(']');
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

fn write_bool_array(output: &mut String, values: &[bool]) {
    output.push('[');
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        output.push_str(if *value { "true" } else { "false" });
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
    fn regression_samples_require_equal_amplification_and_retain_order() {
        let gate = CorrectnessGate { passed: true };
        let mut samples = RegressionTimingSeries::default();
        accept_regression_timed_pair(
            &gate,
            &batch(64.0, 64),
            &batch(32.0, 64),
            64,
            true,
            false,
            &mut samples,
        )
        .expect("symmetric pair");
        assert_eq!(samples.candidate_per_query_ms, [1.0]);
        assert_eq!(samples.baseline_per_query_ms, [0.5]);
        assert_eq!(samples.candidate_first, [false]);

        let error = accept_regression_timed_pair(
            &gate,
            &batch(64.0, 64),
            &batch(32.0, 63),
            64,
            true,
            true,
            &mut samples,
        )
        .expect_err("asymmetric amplification must fail");
        assert!(error.contains("amplification mismatch"));
        assert_eq!(samples.candidate_first, [false]);
    }

    #[test]
    fn candidate_baseline_order_is_counterbalanced() {
        let orders = (0..7)
            .map(|iteration| candidate_runs_first(0, iteration))
            .collect::<Vec<_>>();
        assert_eq!(orders, [true, false, true, false, true, false, true]);
        assert_eq!(orders.iter().filter(|value| **value).count(), 4);
        assert_eq!(orders.iter().filter(|value| !**value).count(), 3);
        assert!(!candidate_runs_first(1, 0));
    }

    #[test]
    fn regression_limits_use_slowdown_not_ratio_point_loss() {
        assert!(!regression_exceeds_limit(1.0 / 1.20, 0.20));
        assert!(regression_exceeds_limit(1.0 / 1.201, 0.20));
        assert!(!regression_exceeds_limit(1.0 / 1.10, 0.10));
        assert!(regression_exceeds_limit(1.0 / 1.101, 0.10));
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
}
