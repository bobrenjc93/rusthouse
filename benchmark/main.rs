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
use dataset::{Dataset, SchemaProfile};
use normalize::{ColumnType, compare_outputs};
use process::{ClickHouseIdentity, Engine, EnginePaths, TimedBatch, TimedOutput};
use score::{RatioObservation, ScoreBreakdown, WorkloadDimension, median, parity_score};
use workload::workloads;

const MAX_SAMPLE_SPREAD: f64 = 10.0;

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
Default mode derives three seeds from the root; quick mode uses only the root.
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
    profile: &'static str,
    seed: u64,
    dataset_seed: u64,
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
    let profiles = SchemaProfile::ALL;
    let expected_profile_names = profiles.map(SchemaProfile::name);
    let seeds = config.mode.seeds(config.seed);
    let expected_workloads = profiles
        .into_iter()
        .flat_map(|profile| {
            workloads(profile, 1)
                .into_iter()
                .map(move |workload| WorkloadDimension {
                    profile: profile.name(),
                    family: workload.family.name(),
                    workload: workload.name,
                })
        })
        .collect::<Vec<_>>();
    let paths = EnginePaths {
        rusthouse: config.rusthouse.clone(),
        clickhouse: config.clickhouse.clone(),
    };
    let identity = paths.validate()?;
    let mut cases = Vec::new();
    let mut correctness_checks = 0_usize;

    for (seed_index, seed) in seeds.iter().copied().enumerate() {
        for (profile_index, profile) in profiles.into_iter().enumerate() {
            for (row_count_index, row_count) in settings.row_counts.iter().copied().enumerate() {
                let dataset_seed = seed
                    ^ (row_count as u64).wrapping_mul(0xd6e8_feb8_6659_fd93)
                    ^ profile.seed_salt();
                let dataset = Dataset::generate(profile, dataset_seed, row_count);
                let setup_sql = dataset.setup_sql();

                for (workload_index, workload) in
                    workloads(profile, row_count).into_iter().enumerate()
                {
                    let query_amplification = partitioned_query_budget(
                        settings.sustained_query_budget,
                        profiles.len() * seeds.len(),
                        seed_index * profiles.len() + profile_index,
                        row_count_index + workload_index,
                    )?;
                    eprintln!(
                        "benchmarking {} / {} at {} rows with seed {} ({}x amplification, {} warmups, {} primary samples, {} end-to-end samples)",
                        profile.name(),
                        workload.name,
                        row_count,
                        seed,
                        query_amplification,
                        settings.warmups,
                        settings.samples,
                        settings.end_to_end_samples
                    );

                    let correctness_order =
                        (seed_index + profile_index + row_count_index + workload_index)
                            .is_multiple_of(2);
                    let (rusthouse_output, clickhouse_output) = execute_correctness_pair(
                        &paths,
                        &setup_sql,
                        &workload.sql,
                        correctness_order,
                    )?;
                    let mut correctness_gate = CorrectnessGate::default();
                    correctness_gate
                        .verify(&workload.columns, &rusthouse_output, &clickhouse_output)
                        .map_err(|error| {
                            format!(
                                "correctness gate failed for '{} / {}' at {row_count} rows with seed {}: {error}",
                                profile.name(), workload.name, seed
                            )
                        })?;
                    correctness_checks += 1;

                    let mut primary = TimingSeries::default();
                    let primary_iterations = settings.warmups + settings.samples;
                    for iteration in 0..primary_iterations {
                        let rusthouse_first = (seed_index
                            + profile_index
                            + row_count_index
                            + workload_index
                            + iteration
                            + 1)
                        .is_multiple_of(2);
                        let (rusthouse, clickhouse) = execute_timed_pair(
                            &paths,
                            &setup_sql,
                            &workload.sql,
                            query_amplification,
                            rusthouse_first,
                        )?;
                        accept_timed_pair(
                            &correctness_gate,
                            &rusthouse,
                            &clickhouse,
                            query_amplification,
                            iteration >= settings.warmups,
                            &mut primary,
                        )?;
                    }

                    let mut end_to_end = TimingSeries::default();
                    for iteration in 0..settings.end_to_end_samples {
                        let rusthouse_first = (seed_index
                            + profile_index
                            + row_count_index
                            + workload_index
                            + iteration
                            + primary_iterations)
                            .is_multiple_of(2);
                        let (rusthouse, clickhouse) = execute_timed_pair(
                            &paths,
                            &setup_sql,
                            &workload.sql,
                            1,
                            rusthouse_first,
                        )?;
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
                        profile.name(),
                        seed,
                        workload.name,
                        row_count,
                    )?;
                    let clickhouse_primary_batch_median = stable_median(
                        &primary.clickhouse_batch_ms,
                        "ClickHouse amplified batch",
                        profile.name(),
                        seed,
                        workload.name,
                        row_count,
                    )?;
                    let rusthouse_primary_median = stable_median(
                        &primary.rusthouse_per_query_ms,
                        "RustHouse amortized query",
                        profile.name(),
                        seed,
                        workload.name,
                        row_count,
                    )?;
                    let clickhouse_primary_median = stable_median(
                        &primary.clickhouse_per_query_ms,
                        "ClickHouse amortized query",
                        profile.name(),
                        seed,
                        workload.name,
                        row_count,
                    )?;
                    let rusthouse_end_to_end_median = stable_median(
                        &end_to_end.rusthouse_batch_ms,
                        "RustHouse end-to-end",
                        profile.name(),
                        seed,
                        workload.name,
                        row_count,
                    )?;
                    let clickhouse_end_to_end_median = stable_median(
                        &end_to_end.clickhouse_batch_ms,
                        "ClickHouse end-to-end",
                        profile.name(),
                        seed,
                        workload.name,
                        row_count,
                    )?;
                    let primary_ratio = clickhouse_primary_median / rusthouse_primary_median;
                    let end_to_end_ratio =
                        clickhouse_end_to_end_median / rusthouse_end_to_end_median;
                    eprintln!(
                        "  primary/query: RustHouse {:.3} ms, ClickHouse {:.3} ms, ratio {:.3}; end-to-end ratio {:.3}",
                        rusthouse_primary_median,
                        clickhouse_primary_median,
                        primary_ratio,
                        end_to_end_ratio
                    );
                    cases.push(CaseResult {
                        profile: profile.name(),
                        seed,
                        dataset_seed,
                        workload: workload.name,
                        family: workload.family.name(),
                        row_count,
                        query_amplification,
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
    }

    let primary_score = score_cases(
        &cases,
        &expected_profile_names,
        &seeds,
        &settings.row_counts,
        &expected_workloads,
        |case| case.primary_ratio,
    )?;
    if config.mode == config::Mode::Default {
        ensure_primary_headroom(&primary_score, cases.len())?;
    }
    let end_to_end_score = score_cases(
        &cases,
        &expected_profile_names,
        &seeds,
        &settings.row_counts,
        &expected_workloads,
        |case| case.end_to_end_ratio,
    )?;
    let minimum_amplification = cases
        .iter()
        .map(|case| case.query_amplification)
        .min()
        .ok_or_else(|| "benchmark produced no cases".to_owned())?;
    let maximum_amplification = cases
        .iter()
        .map(|case| case.query_amplification)
        .max()
        .ok_or_else(|| "benchmark produced no cases".to_owned())?;

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
            "{} separate correctness pairs passed across {} cases, {} schema profiles, {} deterministic seed(s), and {} row counts",
            correctness_checks,
            cases.len(),
            profiles.len(),
            seeds.len(),
            settings.row_counts.len()
        ),
        format!(
            "primary score {:.2}; startup-inclusive end-to-end score {:.2}",
            primary_score.score, end_to_end_score.score
        ),
        format!(
            "primary timing holds a fixed {}-query budget per workload/scale/sample across all profile/seed cells; each symmetric engine batch uses {} or {} identical queries, divides positive batch wall time by its exact repetition count, discards stdout, and performs no startup subtraction",
            settings.sustained_query_budget,
            minimum_amplification,
            maximum_amplification
        ),
        format!(
            "primary parity caps: {}/{} cases; end-to-end parity caps: {}/{} cases",
            primary_score.saturated_cases,
            cases.len(),
            end_to_end_score.saturated_cases,
            cases.len()
        ),
        format!(
            "mode={}, root_seed={}, derived_seeds={:?}, profiles={:?}, warmups={}, primary_samples={}, end_to_end_samples={}; ClickHouse SHA-256={}",
            config.mode.name(),
            config.seed,
            seeds,
            expected_profile_names,
            settings.warmups,
            settings.samples,
            settings.end_to_end_samples,
            identity.sha256
        ),
        format!("ClickHouse identity: {}", identity.version_output),
        "limitation: amplification measures repeated warm in-process work, retains a profile-batch-dependent fraction of startup/setup, and does not model concurrency, durable storage, or network access".to_owned(),
        "aggregation is fail-closed and equally weights workload within profile/seed/family/scale, then scale, family, seed, and schema profile".to_owned(),
    ];
    evidence.extend(cases.iter().map(|case| {
        format!(
            "{} / seed {} / {} / {} rows: primary/query RustHouse {:.3} ms, ClickHouse {:.3} ms, ratio {:.3}; end-to-end RustHouse {:.3} ms, ClickHouse {:.3} ms, ratio {:.3}",
            case.profile,
            case.seed,
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
    expected_profiles: &[&str],
    expected_seeds: &[u64],
    expected_scales: &[usize],
    expected_workloads: &[WorkloadDimension<'_>],
    ratio: impl Fn(&CaseResult) -> f64,
) -> Result<ScoreBreakdown, String> {
    let observations = cases
        .iter()
        .map(|case| RatioObservation {
            profile: case.profile,
            seed: case.seed,
            family: case.family,
            workload: case.workload,
            scale: case.row_count,
            ratio: ratio(case),
        })
        .collect::<Vec<_>>();
    parity_score(
        &observations,
        expected_profiles,
        expected_seeds,
        expected_scales,
        expected_workloads,
    )
}

fn partitioned_query_budget(
    total_budget: usize,
    partition_count: usize,
    partition_index: usize,
    rotation: usize,
) -> Result<usize, String> {
    if partition_count == 0 || partition_index >= partition_count {
        return Err("query budget partitions must be non-empty and in range".to_owned());
    }
    if total_budget < partition_count {
        return Err(format!(
            "sustained query budget {total_budget} cannot give positive work to {partition_count} profile/seed cells"
        ));
    }
    let base = total_budget / partition_count;
    let remainder = total_budget % partition_count;
    let rotated_index = (partition_index + rotation % partition_count) % partition_count;
    Ok(base + usize::from(rotated_index < remainder))
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
    profile: &str,
    seed: u64,
    workload: &str,
    row_count: usize,
) -> Result<f64, String> {
    let value = median(samples)?;
    if value <= 0.0 {
        return Err(format!(
            "timer resolution was insufficient for {engine_metric}, '{profile} / {workload}' at {row_count} rows with seed {seed}"
        ));
    }
    let required = samples.len() / 2 + 1;
    let stable = stable_sample_count(samples, MAX_SAMPLE_SPREAD);
    if stable < required {
        let minimum = samples.iter().copied().fold(f64::INFINITY, f64::min);
        let maximum = samples.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        return Err(format!(
            "unstable timing for {engine_metric}, '{profile} / {workload}' at {row_count} rows with seed {seed}: only {stable}/{} samples form a max/min spread <= {:.2}; all-sample spread {:.2}",
            samples.len(),
            MAX_SAMPLE_SPREAD,
            maximum / minimum,
        ));
    }
    Ok(value)
}

fn stable_sample_count(samples: &[f64], maximum_spread: f64) -> usize {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let mut best = 0;
    let mut start = 0;
    for end in 0..sorted.len() {
        while sorted[end] / sorted[start] > maximum_spread {
            start += 1;
        }
        best = best.max(end - start + 1);
    }
    best
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
    let seeds = config.mode.seeds(config.seed);
    let minimum_amplification = cases
        .iter()
        .map(|case| case.query_amplification)
        .min()
        .unwrap_or(0);
    let maximum_amplification = cases
        .iter()
        .map(|case| case.query_amplification)
        .max()
        .unwrap_or(0);
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
    output.push_str("],\"profiles\":[");
    for (index, profile) in SchemaProfile::ALL.into_iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        output.push_str(&json_string(profile.name()));
    }
    output.push_str("],\"seeds\":[");
    for (index, seed) in seeds.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        write!(output, "{seed}").expect("writing to String cannot fail");
    }
    write!(
        output,
        "],\"aggregation\":{{\"space\":\"log\",\"ratio_floor\":0.01,\"ratio_cap\":1.0,\"hierarchy\":[\"workload\",\"scale\",\"family\",\"seed\",\"profile\"],\"complete_matrix_required\":true}},\"timing_method\":{{\"name\":\"in_process_query_amplification\",\"calibration\":\"fixed_total_profile_seed_budget\",\"sustained_query_budget\":{},\"case_query_amplification_min\":{},\"case_query_amplification_max\":{},\"startup_subtraction\":false,\"correctness_runs_separate\":true,\"stability_gate\":\"strict_majority_window\",\"max_majority_sample_spread\":{MAX_SAMPLE_SPREAD:.1}}},\"correctness_checks\":{correctness_checks},\"rusthouse_path\":{},\"clickhouse_path\":{},\"clickhouse_version\":{},\"clickhouse_sha256\":{},\"limitations\":[{},{}],\"cases\":[",
        settings.sustained_query_budget,
        minimum_amplification,
        maximum_amplification,
        json_string(&config.rusthouse.display().to_string()),
        json_string(&config.clickhouse.display().to_string()),
        json_string(&identity.version_output),
        json_string(&identity.sha256),
        json_string("amplification measures repeated warm in-process work and retains a case-dependent fraction of startup and setup"),
        json_string("synthetic single-process data does not model concurrency, durable storage, networking, joins, nullability, or production compression")
    )
    .expect("writing to String cannot fail");

    for (index, case) in cases.iter().enumerate() {
        if index > 0 {
            output.push(',');
        }
        write!(
            output,
            "{{\"profile\":{},\"seed\":{},\"dataset_seed\":{},\"workload\":{},\"family\":{},\"row_count\":{},\"query_amplification\":{},\"primary\":{{\"rusthouse_batch_median_ms\":{:.6},\"clickhouse_batch_median_ms\":{:.6},\"rusthouse_per_query_median_ms\":{:.6},\"clickhouse_per_query_median_ms\":{:.6},\"clickhouse_rusthouse_ratio\":{:.9},\"rusthouse_batch_samples_ms\":",
            json_string(case.profile),
            case.seed,
            case.dataset_seed,
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
    fn one_outlier_does_not_invalidate_a_stable_median() {
        let value = stable_median(&[1.0, 1.1, 20.0], "engine", "profile", 42, "workload", 10)
            .expect("a strict majority is stable");
        assert_eq!(value, 1.1);
    }

    #[test]
    fn samples_without_a_stable_majority_are_rejected() {
        let error = stable_median(&[1.0, 20.0, 400.0], "engine", "profile", 42, "workload", 10)
            .expect_err("no strict majority fits the stability window");
        assert!(error.contains("unstable timing"));
        assert!(error.contains("profile / workload"));
        assert!(error.contains("seed 42"));
        assert!(error.contains("only 1/3 samples"));
    }

    #[test]
    fn fixed_query_budget_is_rotated_and_fully_allocated() {
        for (partitions, expected_min, expected_max) in [(3, 85, 86), (9, 28, 29)] {
            for rotation in 0..12 {
                let allocations = (0..partitions)
                    .map(|index| {
                        partitioned_query_budget(256, partitions, index, rotation)
                            .expect("allocation")
                    })
                    .collect::<Vec<_>>();
                assert_eq!(allocations.iter().sum::<usize>(), 256);
                assert_eq!(allocations.iter().copied().min(), Some(expected_min));
                assert_eq!(allocations.iter().copied().max(), Some(expected_max));
            }
        }
        assert!(partitioned_query_budget(2, 3, 0, 0).is_err());
        assert!(partitioned_query_budget(256, 0, 0, 0).is_err());
        assert!(partitioned_query_budget(256, 3, 3, 0).is_err());
    }

    #[test]
    fn default_matrix_has_every_profile_seed_scale_case_without_more_query_work() {
        let settings = config::Mode::Default.settings();
        let seeds = config::Mode::Default.seeds(20_260_729);
        let cell_count = SchemaProfile::ALL.len() * seeds.len();
        let workloads_per_cell = workloads(SchemaProfile::NumericHeavy, 1).len();
        assert_eq!(
            cell_count * settings.row_counts.len() * workloads_per_cell,
            216
        );

        for scale_index in 0..settings.row_counts.len() {
            for workload_index in 0..workloads_per_cell {
                let allocated = (0..cell_count)
                    .map(|cell_index| {
                        partitioned_query_budget(
                            settings.sustained_query_budget,
                            cell_count,
                            cell_index,
                            scale_index + workload_index,
                        )
                        .expect("allocation")
                    })
                    .sum::<usize>();
                assert_eq!(allocated, settings.sustained_query_budget);
            }
        }
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
