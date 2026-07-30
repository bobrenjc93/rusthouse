use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Quick,
    Default,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Self::Quick => "quick",
            Self::Default => "default",
        }
    }

    pub fn settings(self) -> BenchmarkSettings {
        match self {
            Self::Quick => BenchmarkSettings {
                row_counts: &QUICK_ROW_COUNTS,
                warmups: 1,
                samples: 3,
                amplification: Amplification::Fixed(256),
                end_to_end_samples: 3,
            },
            Self::Default => BenchmarkSettings {
                row_counts: &DEFAULT_ROW_COUNTS,
                warmups: 2,
                samples: 7,
                amplification: Amplification::RowVisitBudget(DEFAULT_TARGET_ROW_VISITS),
                end_to_end_samples: 3,
            },
        }
    }
}

const QUICK_ROW_COUNTS: [usize; 2] = [256, 2_048];
const DEFAULT_ROW_COUNTS: [usize; 2] = [100_000, 1_000_000];
pub const DEFAULT_TARGET_ROW_VISITS: usize = 16_000_000;

#[derive(Debug, Clone, Copy)]
enum Amplification {
    Fixed(usize),
    RowVisitBudget(usize),
}

#[derive(Debug, Clone, Copy)]
pub struct BenchmarkSettings {
    pub row_counts: &'static [usize],
    pub warmups: usize,
    pub samples: usize,
    amplification: Amplification,
    pub end_to_end_samples: usize,
}

impl BenchmarkSettings {
    pub fn query_amplification(self, row_count: usize) -> usize {
        assert!(row_count > 0, "benchmark row counts must be positive");
        match self.amplification {
            Amplification::Fixed(repetitions) => repetitions,
            Amplification::RowVisitBudget(row_visits) => row_visits.div_ceil(row_count).max(1),
        }
    }

    pub fn target_row_visits(self) -> Option<usize> {
        match self.amplification {
            Amplification::Fixed(_) => None,
            Amplification::RowVisitBudget(row_visits) => Some(row_visits),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub mode: Mode,
    pub seed: u64,
    pub rusthouse: PathBuf,
    pub clickhouse: PathBuf,
    pub details: Option<PathBuf>,
}

pub enum ParseResult {
    Run(Config),
    Help,
}

pub fn parse(
    arguments: impl IntoIterator<Item = String>,
    clickhouse_from_env: Option<String>,
    rusthouse_from_env: Option<String>,
    default_rusthouse: PathBuf,
) -> Result<ParseResult, String> {
    let mut mode = Mode::Default;
    let mut seed = 20_260_729_u64;
    let mut clickhouse = clickhouse_from_env.map(PathBuf::from);
    let mut rusthouse = rusthouse_from_env
        .map(PathBuf::from)
        .unwrap_or(default_rusthouse);
    let mut details = None;
    let mut arguments = arguments.into_iter();

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "-h" | "--help" => return Ok(ParseResult::Help),
            "--quick" => mode = Mode::Quick,
            "--mode" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "--mode requires quick or default".to_owned())?;
                mode = parse_mode(&value)?;
            }
            "--seed" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "--seed requires an unsigned integer".to_owned())?;
                seed = parse_seed(&value)?;
            }
            "--clickhouse" => {
                clickhouse = Some(PathBuf::from(
                    arguments
                        .next()
                        .ok_or_else(|| "--clickhouse requires a path".to_owned())?,
                ));
            }
            "--rusthouse" => {
                rusthouse = PathBuf::from(
                    arguments
                        .next()
                        .ok_or_else(|| "--rusthouse requires a path".to_owned())?,
                );
            }
            "--details" => {
                details = Some(PathBuf::from(
                    arguments
                        .next()
                        .ok_or_else(|| "--details requires a path".to_owned())?,
                ));
            }
            _ if argument.starts_with("--mode=") => {
                mode = parse_mode(&argument["--mode=".len()..])?;
            }
            _ if argument.starts_with("--seed=") => {
                seed = parse_seed(&argument["--seed=".len()..])?;
            }
            _ if argument.starts_with("--clickhouse=") => {
                clickhouse = Some(PathBuf::from(&argument["--clickhouse=".len()..]));
            }
            _ if argument.starts_with("--rusthouse=") => {
                rusthouse = PathBuf::from(&argument["--rusthouse=".len()..]);
            }
            _ if argument.starts_with("--details=") => {
                details = Some(PathBuf::from(&argument["--details=".len()..]));
            }
            _ => return Err(format!("unknown argument {argument:?}; try --help")),
        }
    }

    let clickhouse = clickhouse.ok_or_else(|| {
        "ClickHouse path is required; use --clickhouse PATH or RUSTHOUSE_CLICKHOUSE_BIN".to_owned()
    })?;
    Ok(ParseResult::Run(Config {
        mode,
        seed,
        rusthouse,
        clickhouse,
        details,
    }))
}

fn parse_mode(value: &str) -> Result<Mode, String> {
    match value {
        "quick" => Ok(Mode::Quick),
        "default" => Ok(Mode::Default),
        _ => Err(format!("unknown mode {value:?}; expected quick or default")),
    }
}

fn parse_seed(value: &str) -> Result<u64, String> {
    value
        .parse()
        .map_err(|_| format!("invalid seed {value:?}; expected an unsigned integer"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_overrides_environment_and_accepts_runtime_seed() {
        let ParseResult::Run(config) = parse(
            [
                "--quick",
                "--seed=99",
                "--clickhouse=/command/clickhouse",
                "--rusthouse=/command/rusthouse",
                "--details=details.json",
            ]
            .into_iter()
            .map(str::to_owned),
            Some("/environment/clickhouse".to_owned()),
            None,
            PathBuf::from("/default/rusthouse"),
        )
        .expect("configuration") else {
            panic!("expected run");
        };

        assert_eq!(config.mode, Mode::Quick);
        assert_eq!(config.seed, 99);
        assert_eq!(config.clickhouse, PathBuf::from("/command/clickhouse"));
        assert_eq!(config.rusthouse, PathBuf::from("/command/rusthouse"));
        assert_eq!(config.details, Some(PathBuf::from("details.json")));
    }

    #[test]
    fn modes_use_multiple_row_counts_and_repeated_samples() {
        for mode in [Mode::Quick, Mode::Default] {
            let settings = mode.settings();
            assert!(settings.row_counts.len() >= 2);
            assert!(settings.warmups >= 1);
            assert!(settings.samples >= 3);
            assert!(
                settings
                    .row_counts
                    .iter()
                    .all(|row_count| settings.query_amplification(*row_count) > 1)
            );
            assert!(settings.end_to_end_samples >= 3);
        }
    }

    #[test]
    fn default_scales_derive_equal_work_from_the_fixed_budget() {
        let settings = Mode::Default.settings();
        assert_eq!(settings.row_counts, [100_000, 1_000_000]);
        assert_eq!(
            settings
                .row_counts
                .iter()
                .map(|row_count| settings.query_amplification(*row_count))
                .collect::<Vec<_>>(),
            [160, 16]
        );
        assert_eq!(settings.target_row_visits(), Some(16_000_000));
        assert!(settings.row_counts.iter().all(|row_count| {
            row_count * settings.query_amplification(*row_count) == DEFAULT_TARGET_ROW_VISITS
        }));
    }

    #[test]
    fn clickhouse_path_is_required() {
        let error = match parse(std::iter::empty(), None, None, PathBuf::from("rusthouse")) {
            Ok(_) => panic!("missing ClickHouse path should fail"),
            Err(error) => error,
        };
        assert!(error.contains("RUSTHOUSE_CLICKHOUSE_BIN"));
    }
}
