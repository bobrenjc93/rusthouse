mod config;
mod dataset;
mod digest;
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
use normalize::{ColumnType, compare_output_sequences};
use process::{ClickHouseIdentity, Engine, EnginePaths, TimedBatch, TimedOutput};
use score::{RatioObservation, ScoreBreakdown, median, parity_score};
use workload::{QUERY_SEQUENCE_METHODOLOGY, QuerySequence, repeated_query_sequence, workloads};

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
    row_count: usize,
    query_amplification: usize,
    query_sequence: QuerySequence,
    primary: TimingSeries,
    rusthouse_primary_batch_median_ms: f64,
    clickhouse_primary_batch_median_ms: f64,
    rusthouse_primary_median_ms: f64,
    clickhouse_primary_median_ms: f64,
    primary_ratio: f64,
    identical_query_transition_sha256: String,
    identical_query_transition: TimingSeries,
    rusthouse_identical_query_transition_batch_median_ms: f64,
    clickhouse_identical_query_transition_batch_median_ms: f64,
    rusthouse_identical_query_transition_median_ms: f64,
    clickhouse_identical_query_transition_median_ms: f64,
    identical_query_transition_ratio: f64,
    end_to_end: TimingSeries,
    rusthouse_end_to_end_median_ms: f64,
    clickhouse_end_to_end_median_ms: f64,
    end_to_end_ratio: f64,
}

#[derive(Debug, Default)]
struct CorrectnessGate {
    sequence_sha256: Option<String>,
    query_count: usize,
}

impl CorrectnessGate {
    fn verify(
        &mut self,
        columns: &[(&str, ColumnType)],
        rusthouse: &TimedOutput,
        clickhouse: &TimedOutput,
        sequence: &QuerySequence,
    ) -> Result<(), String> {
        if rusthouse.query_count != sequence.query_count()
            || clickhouse.query_count != sequence.query_count()
            || rusthouse.sequence_sha256 != sequence.sha256
            || clickhouse.sequence_sha256 != sequence.sha256
        {
            return Err(
                "correctness processes did not receive the expected byte-identical query sequence"
                    .to_owned(),
            );
        }
        compare_output_sequences(
            &rusthouse.stdout,
            &clickhouse.stdout,
            columns,
            sequence.query_count(),
        )?;
        self.sequence_sha256 = Some(sequence.sha256.clone());
        self.query_count = sequence.query_count();
        Ok(())
    }
}

struct Report {
    score: f64,
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
    let paths = EnginePaths {
        rusthouse: config.rusthouse.clone(),
        clickhouse: config.clickhouse.clone(),
    };
    let identity = paths.validate()?;
    let mut cases = Vec::new();
    let mut correctness_checks = 0_usize;

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

            let query_sequence =
                workload.query_sequence(row_count, config.seed, settings.query_amplification);
            let identical_query_transition_sequence =
                repeated_query_sequence(&workload.sql, settings.query_amplification);
            let end_to_end_sequence = repeated_query_sequence(&workload.sql, 1);
            for (label, sequence) in [
                ("query-diverse primary", &query_sequence),
                (
                    "identical-query transition",
                    &identical_query_transition_sequence,
                ),
                ("end-to-end", &end_to_end_sequence),
            ] {
                sequence.validate().map_err(|error| {
                    format!(
                        "invalid {label} sequence for '{}' at {row_count} rows: {error}",
                        workload.name
                    )
                })?;
            }

            let correctness_order = (row_count_index + workload_index).is_multiple_of(2);
            let (rusthouse_output, clickhouse_output) =
                execute_correctness_pair(&paths, &setup_sql, &query_sequence, correctness_order)?;
            let mut primary_gate = CorrectnessGate::default();
            primary_gate
                .verify(
                    &workload.columns,
                    &rusthouse_output,
                    &clickhouse_output,
                    &query_sequence,
                )
                .map_err(|error| {
                    format!(
                        "query-diverse correctness gate failed for '{}' at {row_count} rows: {error}",
                        workload.name
                    )
                })?;

            let (rusthouse_output, clickhouse_output) = execute_correctness_pair(
                &paths,
                &setup_sql,
                &identical_query_transition_sequence,
                !correctness_order,
            )?;
            let mut identical_query_transition_gate = CorrectnessGate::default();
            identical_query_transition_gate
                .verify(
                    &workload.columns,
                    &rusthouse_output,
                    &clickhouse_output,
                    &identical_query_transition_sequence,
                )
                .map_err(|error| {
                    format!(
                        "identical-query transition correctness gate failed for '{}' at {row_count} rows: {error}",
                        workload.name
                    )
                })?;

            let (rusthouse_output, clickhouse_output) = execute_correctness_pair(
                &paths,
                &setup_sql,
                &end_to_end_sequence,
                correctness_order,
            )?;
            let mut end_to_end_gate = CorrectnessGate::default();
            end_to_end_gate
                .verify(
                    &workload.columns,
                    &rusthouse_output,
                    &clickhouse_output,
                    &end_to_end_sequence,
                )
                .map_err(|error| {
                    format!(
                        "end-to-end correctness gate failed for '{}' at {row_count} rows: {error}",
                        workload.name
                    )
                })?;
            correctness_checks += 3;

            let mut primary = TimingSeries::default();
            let primary_iterations = settings.warmups + settings.samples;
            for iteration in 0..primary_iterations {
                let rusthouse_first =
                    (row_count_index + workload_index + iteration + 1).is_multiple_of(2);
                let (rusthouse, clickhouse) =
                    execute_timed_pair(&paths, &setup_sql, &query_sequence, rusthouse_first)?;
                accept_timed_pair(
                    &primary_gate,
                    &rusthouse,
                    &clickhouse,
                    &query_sequence,
                    iteration >= settings.warmups,
                    &mut primary,
                )?;
            }

            let mut identical_query_transition = TimingSeries::default();
            for iteration in 0..primary_iterations {
                let rusthouse_first =
                    (row_count_index + workload_index + iteration + primary_iterations)
                        .is_multiple_of(2);
                let (rusthouse, clickhouse) = execute_timed_pair(
                    &paths,
                    &setup_sql,
                    &identical_query_transition_sequence,
                    rusthouse_first,
                )?;
                accept_timed_pair(
                    &identical_query_transition_gate,
                    &rusthouse,
                    &clickhouse,
                    &identical_query_transition_sequence,
                    iteration >= settings.warmups,
                    &mut identical_query_transition,
                )?;
            }

            let mut end_to_end = TimingSeries::default();
            for iteration in 0..settings.end_to_end_samples {
                let rusthouse_first =
                    (row_count_index + workload_index + iteration + primary_iterations * 2)
                        .is_multiple_of(2);
                let (rusthouse, clickhouse) =
                    execute_timed_pair(&paths, &setup_sql, &end_to_end_sequence, rusthouse_first)?;
                accept_timed_pair(
                    &end_to_end_gate,
                    &rusthouse,
                    &clickhouse,
                    &end_to_end_sequence,
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
            let rusthouse_identical_query_transition_batch_median = stable_median(
                &identical_query_transition.rusthouse_batch_ms,
                "RustHouse identical-query transition batch",
                workload.name,
                row_count,
            )?;
            let clickhouse_identical_query_transition_batch_median = stable_median(
                &identical_query_transition.clickhouse_batch_ms,
                "ClickHouse identical-query transition batch",
                workload.name,
                row_count,
            )?;
            let rusthouse_identical_query_transition_median = stable_median(
                &identical_query_transition.rusthouse_per_query_ms,
                "RustHouse identical-query transition amortized query",
                workload.name,
                row_count,
            )?;
            let clickhouse_identical_query_transition_median = stable_median(
                &identical_query_transition.clickhouse_per_query_ms,
                "ClickHouse identical-query transition amortized query",
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
            let identical_query_transition_ratio = clickhouse_identical_query_transition_median
                / rusthouse_identical_query_transition_median;
            let end_to_end_ratio = clickhouse_end_to_end_median / rusthouse_end_to_end_median;
            eprintln!(
                "  diverse/query: RustHouse {:.3} ms, ClickHouse {:.3} ms, ratio {:.3}; identical-query transition ratio {:.3}; end-to-end ratio {:.3}",
                rusthouse_primary_median,
                clickhouse_primary_median,
                primary_ratio,
                identical_query_transition_ratio,
                end_to_end_ratio
            );
            cases.push(CaseResult {
                workload: workload.name,
                family: workload.family.name(),
                row_count,
                query_amplification: settings.query_amplification,
                query_sequence,
                primary,
                rusthouse_primary_batch_median_ms: rusthouse_primary_batch_median,
                clickhouse_primary_batch_median_ms: clickhouse_primary_batch_median,
                rusthouse_primary_median_ms: rusthouse_primary_median,
                clickhouse_primary_median_ms: clickhouse_primary_median,
                primary_ratio,
                identical_query_transition_sha256: identical_query_transition_sequence.sha256,
                identical_query_transition,
                rusthouse_identical_query_transition_batch_median_ms:
                    rusthouse_identical_query_transition_batch_median,
                clickhouse_identical_query_transition_batch_median_ms:
                    clickhouse_identical_query_transition_batch_median,
                rusthouse_identical_query_transition_median_ms:
                    rusthouse_identical_query_transition_median,
                clickhouse_identical_query_transition_median_ms:
                    clickhouse_identical_query_transition_median,
                identical_query_transition_ratio,
                end_to_end,
                rusthouse_end_to_end_median_ms: rusthouse_end_to_end_median,
                clickhouse_end_to_end_median_ms: clickhouse_end_to_end_median,
                end_to_end_ratio,
            });
        }
    }

    let primary_score = score_cases(&cases, |case| case.primary_ratio)?;
    if config.mode == config::Mode::Default {
        ensure_primary_headroom(&primary_score, cases.len())?;
    }
    let identical_query_transition_score =
        score_cases(&cases, |case| case.identical_query_transition_ratio)?;
    let end_to_end_score = score_cases(&cases, |case| case.end_to_end_ratio)?;

    if let Some(path) = &config.details {
        let details = details_json(
            &config,
            &identity,
            &cases,
            primary_score,
            identical_query_transition_score,
            end_to_end_score,
            correctness_checks,
        );
        fs::write(path, details)
            .map_err(|error| format!("could not write details to '{}': {error}", path.display()))?;
    }

    let mut evidence = vec![
        format!(
            "{} sequence-level correctness pairs passed across {} cases and {} row counts",
            correctness_checks,
            cases.len(),
            settings.row_counts.len()
        ),
        format!(
            "query-diverse primary score {:.2}; identical-query transition score {:.2}; startup-inclusive end-to-end score {:.2}",
            primary_score.score, identical_query_transition_score.score, end_to_end_score.score
        ),
        format!(
            "{} primary timing uses setup plus {} seed-derived query variants per process, divides positive batch wall time by {}, discards timed stdout, and performs no startup subtraction",
            QUERY_SEQUENCE_METHODOLOGY, settings.query_amplification, settings.query_amplification
        ),
        format!(
            "primary parity caps: {}/{} cases; identical-query transition parity caps: {}/{} cases; end-to-end parity caps: {}/{} cases",
            primary_score.saturated_cases,
            cases.len(),
            identical_query_transition_score.saturated_cases,
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
            identity.sha256
        ),
        format!("ClickHouse identity: {}", identity.version_output),
        format!(
            "limitation: amplification measures warm in-process work, retains 1/{} of startup/setup, and does not model cold caches, concurrency, durable storage, or network access",
            settings.query_amplification
        ),
    ];
    evidence.extend(cases.iter().map(|case| {
        format!(
            "{} / {} rows / sequence {}: diverse/query RustHouse {:.3} ms, ClickHouse {:.3} ms, ratio {:.3}; identical-query transition ratio {:.3}; end-to-end RustHouse {:.3} ms, ClickHouse {:.3} ms, ratio {:.3}",
            case.workload,
            case.row_count,
            case.query_sequence.sha256,
            case.rusthouse_primary_median_ms,
            case.clickhouse_primary_median_ms,
            case.primary_ratio,
            case.identical_query_transition_ratio,
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
            "RustHouse query-diverse sustained-work score {:.2}; identical-query transition score {:.2}; startup-inclusive end-to-end score {:.2}; ClickHouse parity=100 over {} correctness-gated cases.",
            primary_score.score,
            identical_query_transition_score.score,
            end_to_end_score.score,
            cases.len()
        ),
        evidence,
        suggestions,
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
    sequence: &QuerySequence,
    rusthouse_first: bool,
) -> Result<(TimedOutput, TimedOutput), String> {
    if rusthouse_first {
        let rusthouse = paths.execute_correctness(
            Engine::RustHouse,
            setup_sql,
            &sequence.sql,
            sequence.query_count(),
        )?;
        let clickhouse = paths.execute_correctness(
            Engine::ClickHouse,
            setup_sql,
            &sequence.sql,
            sequence.query_count(),
        )?;
        Ok((rusthouse, clickhouse))
    } else {
        let clickhouse = paths.execute_correctness(
            Engine::ClickHouse,
            setup_sql,
            &sequence.sql,
            sequence.query_count(),
        )?;
        let rusthouse = paths.execute_correctness(
            Engine::RustHouse,
            setup_sql,
            &sequence.sql,
            sequence.query_count(),
        )?;
        Ok((rusthouse, clickhouse))
    }
}

fn execute_timed_pair(
    paths: &EnginePaths,
    setup_sql: &str,
    sequence: &QuerySequence,
    rusthouse_first: bool,
) -> Result<(TimedBatch, TimedBatch), String> {
    if rusthouse_first {
        let rusthouse = paths.execute_timed(
            Engine::RustHouse,
            setup_sql,
            &sequence.sql,
            sequence.query_count(),
        )?;
        let clickhouse = paths.execute_timed(
            Engine::ClickHouse,
            setup_sql,
            &sequence.sql,
            sequence.query_count(),
        )?;
        Ok((rusthouse, clickhouse))
    } else {
        let clickhouse = paths.execute_timed(
            Engine::ClickHouse,
            setup_sql,
            &sequence.sql,
            sequence.query_count(),
        )?;
        let rusthouse = paths.execute_timed(
            Engine::RustHouse,
            setup_sql,
            &sequence.sql,
            sequence.query_count(),
        )?;
        Ok((rusthouse, clickhouse))
    }
}

fn accept_timed_pair(
    gate: &CorrectnessGate,
    rusthouse: &TimedBatch,
    clickhouse: &TimedBatch,
    sequence: &QuerySequence,
    record: bool,
    samples: &mut TimingSeries,
) -> Result<(), String> {
    if gate.sequence_sha256.as_deref() != Some(&sequence.sha256)
        || gate.query_count != sequence.query_count()
    {
        return Err("timed batch was not preceded by a passing correctness run".to_owned());
    }
    if rusthouse.query_count != clickhouse.query_count
        || rusthouse.query_count != sequence.query_count()
        || rusthouse.sequence_sha256 != clickhouse.sequence_sha256
        || rusthouse.sequence_sha256 != sequence.sha256
    {
        return Err(format!(
            "query sequence mismatch: expected {} queries with digest {}, RustHouse used {} / {}, ClickHouse used {} / {}",
            sequence.query_count(),
            sequence.sha256,
            rusthouse.query_count,
            rusthouse.sequence_sha256,
            clickhouse.query_count,
            clickhouse.sequence_sha256
        ));
    }

    let rusthouse_batch_ms = rusthouse.elapsed.as_secs_f64() * 1_000.0;
    let clickhouse_batch_ms = clickhouse.elapsed.as_secs_f64() * 1_000.0;
    let rusthouse_per_query_ms = per_query_millis(rusthouse_batch_ms, rusthouse.query_count)?;
    let clickhouse_per_query_ms = per_query_millis(clickhouse_batch_ms, clickhouse.query_count)?;

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
    identical_query_transition_score: ScoreBreakdown,
    end_to_end_score: ScoreBreakdown,
    correctness_checks: usize,
) -> String {
    let settings = config.mode.settings();
    let mut output = String::new();
    write!(
        output,
        "{{\"schema_version\":3,\"score\":{:.6},\"primary_score\":{:.6},\"identical_query_transition_score\":{:.6},\"end_to_end_score\":{:.6},\"primary_saturated_cases\":{},\"identical_query_transition_saturated_cases\":{},\"end_to_end_saturated_cases\":{},\"mode\":{},\"seed\":{},\"warmups\":{},\"primary_samples\":{},\"end_to_end_samples\":{},\"row_counts\":[",
        primary_score.score,
        primary_score.score,
        identical_query_transition_score.score,
        end_to_end_score.score,
        primary_score.saturated_cases,
        identical_query_transition_score.saturated_cases,
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
        "],\"timing_method\":{{\"version\":3,\"name\":{},\"calibration\":\"fixed_shared_repetitions\",\"query_amplification\":{},\"sequence_digest\":\"sha256\",\"complete_amplified_output_check\":true,\"byte_identical_sequences\":true,\"startup_subtraction\":false,\"correctness_runs_separate\":true,\"max_sample_spread\":{MAX_SAMPLE_SPREAD:.1}}},\"transition_metric\":{{\"name\":\"identical_query_amplification_v2\",\"label\":\"non_primary_transition_only\",\"query_amplification\":{}}},\"correctness_checks\":{correctness_checks},\"rusthouse_path\":{},\"clickhouse_path\":{},\"clickhouse_version\":{},\"clickhouse_sha256\":{},\"limitations\":[{},{},{}],\"cases\":[",
        json_string(QUERY_SEQUENCE_METHODOLOGY),
        settings.query_amplification,
        settings.query_amplification,
        json_string(&config.rusthouse.display().to_string()),
        json_string(&config.clickhouse.display().to_string()),
        json_string(&identity.version_output),
        json_string(&identity.sha256),
        json_string("query-diverse amplification measures warm in-process work, does not model cold caches, and retains one divided by the amplification factor of startup and setup"),
        json_string("the identical-query score is retained only as a transition metric and is not the reported Burner score"),
        json_string("synthetic single-process data does not model concurrency, durable storage, networking, joins, nullability, or production compression")
    )
    .expect("writing to String cannot fail");

    for (index, case) in cases.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        write!(
            output,
            "{{\"workload\":{},\"family\":{},\"row_count\":{},\"query_amplification\":{},\"query_sequence\":{{\"seed\":{},\"sha256\":{},\"resolved_parameters\":",
            json_string(case.workload),
            json_string(case.family),
            case.row_count,
            case.query_amplification,
            case.query_sequence.seed,
            json_string(&case.query_sequence.sha256),
        )
        .expect("writing to String cannot fail");
        write_resolved_variants(&mut output, &case.query_sequence);
        write!(
            output,
            "}},\"primary\":{{\"rusthouse_batch_median_ms\":{:.6},\"clickhouse_batch_median_ms\":{:.6},\"rusthouse_per_query_median_ms\":{:.6},\"clickhouse_per_query_median_ms\":{:.6},\"clickhouse_rusthouse_ratio\":{:.9},\"rusthouse_batch_samples_ms\":",
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
            "}},\"identical_query_transition\":{{\"sequence_sha256\":{},\"rusthouse_batch_median_ms\":{:.6},\"clickhouse_batch_median_ms\":{:.6},\"rusthouse_per_query_median_ms\":{:.6},\"clickhouse_per_query_median_ms\":{:.6},\"clickhouse_rusthouse_ratio\":{:.9},\"rusthouse_batch_samples_ms\":",
            json_string(&case.identical_query_transition_sha256),
            case.rusthouse_identical_query_transition_batch_median_ms,
            case.clickhouse_identical_query_transition_batch_median_ms,
            case.rusthouse_identical_query_transition_median_ms,
            case.clickhouse_identical_query_transition_median_ms,
            case.identical_query_transition_ratio,
        )
        .expect("writing to String cannot fail");
        write_number_array(
            &mut output,
            &case.identical_query_transition.rusthouse_batch_ms,
        );
        output.push_str(",\"clickhouse_batch_samples_ms\":");
        write_number_array(
            &mut output,
            &case.identical_query_transition.clickhouse_batch_ms,
        );
        output.push_str(",\"rusthouse_per_query_samples_ms\":");
        write_number_array(
            &mut output,
            &case.identical_query_transition.rusthouse_per_query_ms,
        );
        output.push_str(",\"clickhouse_per_query_samples_ms\":");
        write_number_array(
            &mut output,
            &case.identical_query_transition.clickhouse_per_query_ms,
        );
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

fn write_resolved_variants(output: &mut String, sequence: &QuerySequence) {
    output.push('[');
    for (variant_index, variant) in sequence.variants.iter().enumerate() {
        if variant_index > 0 {
            output.push(',');
        }
        write!(output, "{{\"ordinal\":{}", variant.ordinal).expect("writing to String cannot fail");
        for (name, value) in &variant.parameters {
            write!(output, ",{}:{}", json_string(name), json_string(value))
                .expect("writing to String cannot fail");
        }
        output.push('}');
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

    fn sequence(count: usize) -> QuerySequence {
        repeated_query_sequence("SELECT 1 AS n;", count)
    }

    fn batch(milliseconds: f64, sequence: &QuerySequence) -> TimedBatch {
        TimedBatch {
            elapsed: Duration::from_secs_f64(milliseconds / 1_000.0),
            query_count: sequence.query_count(),
            sequence_sha256: sequence.sha256.clone(),
        }
    }

    fn output(csv: &str, sequence: &QuerySequence) -> TimedOutput {
        TimedOutput {
            stdout: csv.to_owned(),
            query_count: sequence.query_count(),
            sequence_sha256: sequence.sha256.clone(),
        }
    }

    #[test]
    fn correctness_failure_leaves_timing_gate_closed() {
        let sequence = sequence(1);
        let mut gate = CorrectnessGate::default();
        let error = gate
            .verify(
                &[("n", ColumnType::Integer)],
                &output("n\n1\n", &sequence),
                &output("n\n2\n", &sequence),
                &sequence,
            )
            .expect_err("mismatch must fail");

        assert!(error.contains("result mismatch"));
        assert!(gate.sequence_sha256.is_none());
    }

    #[test]
    fn timed_batches_require_a_correctness_gate() {
        let sequence = sequence(64);
        let mut samples = TimingSeries::default();
        let error = accept_timed_pair(
            &CorrectnessGate::default(),
            &batch(10.0, &sequence),
            &batch(10.0, &sequence),
            &sequence,
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
        let sequence = sequence(64);
        let gate = CorrectnessGate {
            sequence_sha256: Some(sequence.sha256.clone()),
            query_count: sequence.query_count(),
        };
        let mut clickhouse = batch(10.0, &sequence);
        clickhouse.query_count = 63;
        let mut samples = TimingSeries::default();
        let error = accept_timed_pair(
            &gate,
            &batch(10.0, &sequence),
            &clickhouse,
            &sequence,
            true,
            &mut samples,
        )
        .expect_err("different amplification must fail");

        assert!(error.contains("query sequence mismatch"));
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
        let sequence = sequence(64);
        let mut gate = CorrectnessGate::default();
        gate.verify(
            &[("enabled", ColumnType::Boolean)],
            &output(&"enabled\ntrue\n".repeat(64), &sequence),
            &output(&"enabled\n1\n".repeat(64), &sequence),
            &sequence,
        )
        .expect("matching output");

        let mut samples = TimingSeries::default();
        accept_timed_pair(
            &gate,
            &batch(64.0, &sequence),
            &batch(32.0, &sequence),
            &sequence,
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
