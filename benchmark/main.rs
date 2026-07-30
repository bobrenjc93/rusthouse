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
use digest::sha256_hex;
use normalize::{ColumnType, compare_outputs, validate_amplified_output};
use process::{CapturedBatch, ClickHouseIdentity, Engine, EnginePaths, TimedBatch, TimedOutput};
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
    amplified_validation: AmplifiedValidation,
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

#[derive(Debug)]
struct EngineValidation {
    validated_repetitions: usize,
    single_query_output_sha256: String,
    amplified_output_sha256: String,
}

#[derive(Debug)]
struct AmplifiedValidation {
    expected_repetitions: usize,
    rusthouse: EngineValidation,
    clickhouse: EngineValidation,
}

#[derive(Debug, Default)]
struct CorrectnessGate {
    single_query_passed: bool,
    amplified_validation_passed: bool,
}

impl CorrectnessGate {
    fn verify_single_query(
        &mut self,
        columns: &[(&str, ColumnType)],
        rusthouse: &TimedOutput,
        clickhouse: &TimedOutput,
    ) -> Result<(), String> {
        compare_outputs(&rusthouse.stdout, &clickhouse.stdout, columns)?;
        self.single_query_passed = true;
        Ok(())
    }

    fn verify_amplified(
        &mut self,
        columns: &[(&str, ColumnType)],
        rusthouse_single: &TimedOutput,
        clickhouse_single: &TimedOutput,
        rusthouse_amplified: &CapturedBatch,
        clickhouse_amplified: &CapturedBatch,
        expected_repetitions: usize,
    ) -> Result<AmplifiedValidation, String> {
        if !self.single_query_passed {
            return Err(
                "amplified validation was not preceded by a passing single-query comparison"
                    .to_owned(),
            );
        }
        if rusthouse_amplified.query_repetitions != expected_repetitions
            || clickhouse_amplified.query_repetitions != expected_repetitions
        {
            return Err(format!(
                "amplified validation count mismatch: expected {expected_repetitions}, RustHouse used {}, ClickHouse used {}",
                rusthouse_amplified.query_repetitions, clickhouse_amplified.query_repetitions
            ));
        }

        let rusthouse_repetitions = validate_amplified_output(
            &rusthouse_single.stdout,
            &rusthouse_amplified.stdout,
            columns,
            "RustHouse",
            expected_repetitions,
        )?;
        let clickhouse_repetitions = validate_amplified_output(
            &clickhouse_single.stdout,
            &clickhouse_amplified.stdout,
            columns,
            "ClickHouse",
            expected_repetitions,
        )?;

        self.amplified_validation_passed = true;
        Ok(AmplifiedValidation {
            expected_repetitions,
            rusthouse: EngineValidation {
                validated_repetitions: rusthouse_repetitions,
                single_query_output_sha256: sha256_hex(rusthouse_single.stdout.as_bytes()),
                amplified_output_sha256: sha256_hex(rusthouse_amplified.stdout.as_bytes()),
            },
            clickhouse: EngineValidation {
                validated_repetitions: clickhouse_repetitions,
                single_query_output_sha256: sha256_hex(clickhouse_single.stdout.as_bytes()),
                amplified_output_sha256: sha256_hex(clickhouse_amplified.stdout.as_bytes()),
            },
        })
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

            let correctness_order = (row_count_index + workload_index).is_multiple_of(2);
            let (rusthouse_output, clickhouse_output) =
                execute_correctness_pair(&paths, &setup_sql, &workload.sql, correctness_order)?;
            let mut correctness_gate = CorrectnessGate::default();
            correctness_gate
                .verify_single_query(&workload.columns, &rusthouse_output, &clickhouse_output)
                .map_err(|error| {
                    format!(
                        "correctness gate failed for '{}' at {row_count} rows: {error}",
                        workload.name
                    )
                })?;
            correctness_checks += 1;

            let (rusthouse_validation, clickhouse_validation) = execute_validation_pair(
                &paths,
                &setup_sql,
                &workload.sql,
                settings.query_amplification,
                !correctness_order,
            )?;
            let amplified_validation = correctness_gate
                .verify_amplified(
                    &workload.columns,
                    &rusthouse_output,
                    &clickhouse_output,
                    &rusthouse_validation,
                    &clickhouse_validation,
                    settings.query_amplification,
                )
                .map_err(|error| {
                    format!(
                        "amplified validation failed for '{}' at {row_count} rows: {error}",
                        workload.name
                    )
                })?;

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
                amplified_validation,
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

    let primary_score = score_cases(&cases, |case| case.primary_ratio)?;
    if config.mode == config::Mode::Default {
        ensure_primary_headroom(&primary_score, cases.len())?;
    }
    let end_to_end_score = score_cases(&cases, |case| case.end_to_end_ratio)?;

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
            "{} single-query correctness pairs and {} captured amplified engine batches passed across {} cases and {} row counts",
            correctness_checks,
            cases.len() * 2,
            cases.len(),
            settings.row_counts.len()
        ),
        format!(
            "validated {} repeated query results before timing; details retain per-engine counts and CSV SHA-256 digests",
            cases
                .iter()
                .map(
                    |case| case.amplified_validation.rusthouse.validated_repetitions
                        + case.amplified_validation.clickhouse.validated_repetitions
                )
                .sum::<usize>()
        ),
        format!(
            "primary score {:.2}; startup-inclusive end-to-end score {:.2}",
            primary_score.score, end_to_end_score.score
        ),
        format!(
            "after separate captured validation, primary timing uses setup plus {} identical queries per process, divides positive batch wall time by {}, discards stdout, and performs no startup subtraction",
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

fn execute_validation_pair(
    paths: &EnginePaths,
    setup_sql: &str,
    query_sql: &str,
    query_repetitions: usize,
    rusthouse_first: bool,
) -> Result<(CapturedBatch, CapturedBatch), String> {
    if rusthouse_first {
        let rusthouse =
            paths.execute_validation(Engine::RustHouse, setup_sql, query_sql, query_repetitions)?;
        let clickhouse = paths.execute_validation(
            Engine::ClickHouse,
            setup_sql,
            query_sql,
            query_repetitions,
        )?;
        Ok((rusthouse, clickhouse))
    } else {
        let clickhouse = paths.execute_validation(
            Engine::ClickHouse,
            setup_sql,
            query_sql,
            query_repetitions,
        )?;
        let rusthouse =
            paths.execute_validation(Engine::RustHouse, setup_sql, query_sql, query_repetitions)?;
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
    if !gate.single_query_passed || !gate.amplified_validation_passed {
        return Err(
            "timed batch was not preceded by passing single-query and amplified validation runs"
                .to_owned(),
        );
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
    let amplified_validation_repetitions = cases
        .iter()
        .map(|case| {
            case.amplified_validation.rusthouse.validated_repetitions
                + case.amplified_validation.clickhouse.validated_repetitions
        })
        .sum::<usize>();
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
        "],\"timing_method\":{{\"name\":\"in_process_query_amplification\",\"calibration\":\"fixed_shared_repetitions\",\"query_amplification\":{},\"startup_subtraction\":false,\"correctness_runs_separate\":true,\"amplified_validation_before_timing\":true,\"max_sample_spread\":{MAX_SAMPLE_SPREAD:.1}}},\"correctness_checks\":{correctness_checks},\"amplified_validation_batches\":{},\"amplified_validation_repetitions\":{amplified_validation_repetitions},\"rusthouse_path\":{},\"clickhouse_path\":{},\"clickhouse_version\":{},\"clickhouse_sha256\":{},\"limitations\":[{},{}],\"cases\":[",
        settings.query_amplification,
        cases.len() * 2,
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
            "{{\"workload\":{},\"family\":{},\"row_count\":{},\"query_amplification\":{},\"amplified_validation\":{{\"expected_repetitions_per_engine\":{},\"rusthouse\":{{\"validated_repetitions\":{},\"single_query_output_sha256\":{},\"amplified_output_sha256\":{}}},\"clickhouse\":{{\"validated_repetitions\":{},\"single_query_output_sha256\":{},\"amplified_output_sha256\":{}}}}},\"primary\":{{\"rusthouse_batch_median_ms\":{:.6},\"clickhouse_batch_median_ms\":{:.6},\"rusthouse_per_query_median_ms\":{:.6},\"clickhouse_per_query_median_ms\":{:.6},\"clickhouse_rusthouse_ratio\":{:.9},\"rusthouse_batch_samples_ms\":",
            json_string(case.workload),
            json_string(case.family),
            case.row_count,
            case.query_amplification,
            case.amplified_validation.expected_repetitions,
            case.amplified_validation.rusthouse.validated_repetitions,
            json_string(
                &case
                    .amplified_validation
                    .rusthouse
                    .single_query_output_sha256
            ),
            json_string(
                &case
                    .amplified_validation
                    .rusthouse
                    .amplified_output_sha256
            ),
            case.amplified_validation.clickhouse.validated_repetitions,
            json_string(
                &case
                    .amplified_validation
                    .clickhouse
                    .single_query_output_sha256
            ),
            json_string(
                &case
                    .amplified_validation
                    .clickhouse
                    .amplified_output_sha256
            ),
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
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;
    #[cfg(unix)]
    use std::sync::atomic::{AtomicU64, Ordering};

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

    fn captured(csv: &str, repetitions: usize) -> CapturedBatch {
        CapturedBatch {
            stdout: csv.to_owned(),
            query_repetitions: repetitions,
        }
    }

    fn open_gate() -> CorrectnessGate {
        CorrectnessGate {
            single_query_passed: true,
            amplified_validation_passed: true,
        }
    }

    #[cfg(unix)]
    struct FakeEngine {
        directory: PathBuf,
        executable: PathBuf,
    }

    #[cfg(unix)]
    impl FakeEngine {
        fn new(stdout: &str) -> Self {
            static NEXT_ID: AtomicU64 = AtomicU64::new(0);
            let directory = env::temp_dir().join(format!(
                "rusthouse-fake-engine-{}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&directory).expect("create fake-engine directory");
            let executable = directory.join("engine");
            let escaped = stdout.replace('\'', "'\\''");
            fs::write(
                &executable,
                format!("#!/bin/sh\ncat >/dev/null\nprintf '%s' '{escaped}'\n"),
            )
            .expect("write fake engine");
            let mut permissions = fs::metadata(&executable)
                .expect("fake-engine metadata")
                .permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&executable, permissions).expect("make fake engine executable");
            Self {
                directory,
                executable,
            }
        }

        fn paths(&self) -> EnginePaths {
            EnginePaths {
                rusthouse: self.executable.clone(),
                clickhouse: self.executable.clone(),
            }
        }
    }

    #[cfg(unix)]
    impl Drop for FakeEngine {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.directory).expect("remove fake-engine directory");
        }
    }

    #[test]
    fn correctness_failure_leaves_timing_gate_closed() {
        let mut gate = CorrectnessGate::default();
        let error = gate
            .verify_single_query(
                &[("n", ColumnType::Integer)],
                &output("n\n1\n"),
                &output("n\n2\n"),
            )
            .expect_err("mismatch must fail");

        assert!(error.contains("result mismatch"));
        assert!(!gate.single_query_passed);
        assert!(!gate.amplified_validation_passed);
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

        assert!(error.contains("single-query"));
        assert!(samples.rusthouse_batch_ms.is_empty());
        assert!(samples.clickhouse_batch_ms.is_empty());
    }

    #[test]
    fn amplification_must_match_for_both_engines() {
        let mut samples = TimingSeries::default();
        let error = accept_timed_pair(
            &open_gate(),
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
        let rusthouse_single = output("enabled\ntrue\n");
        let clickhouse_single = output("enabled\n1\n");
        gate.verify_single_query(
            &[("enabled", ColumnType::Boolean)],
            &rusthouse_single,
            &clickhouse_single,
        )
        .expect("matching output");
        gate.verify_amplified(
            &[("enabled", ColumnType::Boolean)],
            &rusthouse_single,
            &clickhouse_single,
            &captured("enabled\ntrue\n\nenabled\ntrue\n", 2),
            &captured("enabled\n1\nenabled\n1\n", 2),
            2,
        )
        .expect("matching amplified output");

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
    #[cfg(unix)]
    fn adversarial_fake_engine_cannot_emit_only_one_result_for_an_amplified_batch() {
        let fake = FakeEngine::new("n\n1\n");
        let paths = fake.paths();
        let single = paths
            .execute_correctness(Engine::RustHouse, "", "SELECT 1 AS n;")
            .expect("single fake-engine query");
        let amplified = paths
            .execute_validation(Engine::RustHouse, "", "SELECT 1 AS n;", 3)
            .expect("amplified fake-engine query");

        let error = validate_amplified_output(
            &single.stdout,
            &amplified.stdout,
            &[("n", ColumnType::Integer)],
            "fake engine",
            3,
        )
        .expect_err("one result must not stand in for three");
        assert!(error.contains("missing repetition 2"));
    }

    #[test]
    #[cfg(unix)]
    fn adversarial_fake_engines_cannot_reorder_rows_or_append_results() {
        let cases = [
            ("n\n1\n2\nn\n2\n1\nn\n1\n2\n", "reordered rows must fail"),
            (
                "n\n1\n2\nn\n1\n2\nn\n1\n2\nn\n1\n2\n",
                "extra result must fail",
            ),
        ];
        for (stdout, message) in cases {
            let fake = FakeEngine::new(stdout);
            let amplified = fake
                .paths()
                .execute_validation(Engine::RustHouse, "", "SELECT n FROM t;", 3)
                .expect("amplified fake-engine query");
            assert!(
                validate_amplified_output(
                    "n\n1\n2\n",
                    &amplified.stdout,
                    &[("n", ColumnType::Integer)],
                    "fake engine",
                    3,
                )
                .is_err(),
                "{message}"
            );
        }
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
    fn details_record_amplified_validation_counts_and_digests() {
        let series = || TimingSeries {
            rusthouse_batch_ms: vec![2.0],
            clickhouse_batch_ms: vec![1.0],
            rusthouse_per_query_ms: vec![1.0],
            clickhouse_per_query_ms: vec![0.5],
        };
        let rusthouse_digest = sha256_hex(b"rusthouse output");
        let clickhouse_digest = sha256_hex(b"clickhouse output");
        let cases = [CaseResult {
            workload: "test workload",
            family: "test family",
            row_count: 256,
            query_amplification: 3,
            amplified_validation: AmplifiedValidation {
                expected_repetitions: 3,
                rusthouse: EngineValidation {
                    validated_repetitions: 3,
                    single_query_output_sha256: rusthouse_digest.clone(),
                    amplified_output_sha256: rusthouse_digest.clone(),
                },
                clickhouse: EngineValidation {
                    validated_repetitions: 3,
                    single_query_output_sha256: clickhouse_digest.clone(),
                    amplified_output_sha256: clickhouse_digest.clone(),
                },
            },
            primary: series(),
            rusthouse_primary_batch_median_ms: 2.0,
            clickhouse_primary_batch_median_ms: 1.0,
            rusthouse_primary_median_ms: 1.0,
            clickhouse_primary_median_ms: 0.5,
            primary_ratio: 0.5,
            end_to_end: series(),
            rusthouse_end_to_end_median_ms: 2.0,
            clickhouse_end_to_end_median_ms: 1.0,
            end_to_end_ratio: 0.5,
        }];
        let details = details_json(
            &Config {
                mode: config::Mode::Quick,
                seed: 1,
                rusthouse: PathBuf::from("rusthouse"),
                clickhouse: PathBuf::from("clickhouse"),
                details: None,
            },
            &ClickHouseIdentity {
                version_output: "26.7.1".to_owned(),
                sha256: "reference digest".to_owned(),
            },
            &cases,
            ScoreBreakdown {
                score: 50.0,
                saturated_cases: 0,
            },
            ScoreBreakdown {
                score: 50.0,
                saturated_cases: 0,
            },
            1,
        );

        assert!(details.contains("\"schema_version\":3"));
        assert!(details.contains("\"amplified_validation_batches\":2"));
        assert!(details.contains("\"amplified_validation_repetitions\":6"));
        assert!(details.contains(&format!(
            "\"single_query_output_sha256\":{}",
            json_string(&rusthouse_digest)
        )));
        assert!(details.contains(&format!(
            "\"amplified_output_sha256\":{}",
            json_string(&clickhouse_digest)
        )));
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
