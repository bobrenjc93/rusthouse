use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Quick,
    Default,
    Audit,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Self::Quick => "quick",
            Self::Default => "default",
            Self::Audit => "audit",
        }
    }

    pub fn settings(self) -> BenchmarkSettings {
        match self {
            Self::Quick => BenchmarkSettings {
                scales: vec![ScaleSettings::new(256, 256), ScaleSettings::new(2_048, 256)],
                warmups: 1,
                samples: 3,
                end_to_end_samples: 3,
            },
            Self::Default => BenchmarkSettings {
                scales: vec![
                    ScaleSettings::new(1_000, 256),
                    ScaleSettings::new(10_000, 256),
                    ScaleSettings::new(50_000, 256),
                ],
                warmups: 2,
                samples: 7,
                end_to_end_samples: 3,
            },
            Self::Audit => BenchmarkSettings {
                scales: vec![
                    ScaleSettings::new(50_000, 256),
                    ScaleSettings::new(250_000, 64),
                    ScaleSettings::new(1_000_000, 16),
                ],
                warmups: 2,
                samples: 5,
                end_to_end_samples: 3,
            },
        }
    }

    pub fn seeds(self, base_seed: u64) -> Vec<u64> {
        match self {
            Self::Quick | Self::Default => vec![base_seed],
            Self::Audit => (1..=3)
                .map(|index| {
                    splitmix64_output(
                        base_seed.wrapping_add(0x9e37_79b9_7f4a_7c15_u64.wrapping_mul(index)),
                    )
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct BenchmarkSettings {
    pub scales: Vec<ScaleSettings>,
    pub warmups: usize,
    pub samples: usize,
    pub end_to_end_samples: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScaleSettings {
    pub row_count: usize,
    pub query_amplification: usize,
}

impl ScaleSettings {
    const fn new(row_count: usize, query_amplification: usize) -> Self {
        Self {
            row_count,
            query_amplification,
        }
    }
}

fn splitmix64_output(mut value: u64) -> u64 {
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
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
                    .ok_or_else(|| "--mode requires quick, default, or audit".to_owned())?;
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
        "audit" => Ok(Mode::Audit),
        _ => Err(format!(
            "unknown mode {value:?}; expected quick, default, or audit"
        )),
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
        for mode in [Mode::Quick, Mode::Default, Mode::Audit] {
            let settings = mode.settings();
            assert!(settings.scales.len() >= 2);
            assert!(settings.warmups >= 1);
            assert!(settings.samples >= 3);
            assert!(
                settings
                    .scales
                    .iter()
                    .all(|scale| scale.query_amplification > 1)
            );
            assert!(settings.end_to_end_samples >= 3);
        }
    }

    #[test]
    fn audit_mode_is_accepted_from_the_command_line() {
        let ParseResult::Run(config) = parse(
            ["--mode=audit", "--clickhouse=/clickhouse"]
                .into_iter()
                .map(str::to_owned),
            None,
            None,
            PathBuf::from("/rusthouse"),
        )
        .expect("audit configuration") else {
            panic!("expected run");
        };
        assert_eq!(config.mode, Mode::Audit);
    }

    #[test]
    fn audit_uses_three_derived_seeds_and_reaches_one_million_rows() {
        let seeds = Mode::Audit.seeds(99);
        assert_eq!(seeds.len(), 3);
        assert_eq!(seeds, Mode::Audit.seeds(99));
        assert_ne!(seeds, Mode::Audit.seeds(100));
        assert_eq!(
            seeds
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            3
        );

        let settings = Mode::Audit.settings();
        assert_eq!(settings.scales.last().expect("scale").row_count, 1_000_000);
        assert_eq!(
            settings
                .scales
                .iter()
                .map(|scale| (scale.row_count, scale.query_amplification))
                .collect::<Vec<_>>(),
            [(50_000, 256), (250_000, 64), (1_000_000, 16)]
        );
        assert!(
            settings
                .scales
                .windows(2)
                .all(|pair| pair[0].query_amplification > pair[1].query_amplification)
        );
    }

    #[test]
    fn quick_and_default_keep_their_original_calibration() {
        assert_eq!(
            Mode::Quick
                .settings()
                .scales
                .iter()
                .map(|scale| (scale.row_count, scale.query_amplification))
                .collect::<Vec<_>>(),
            [(256, 256), (2_048, 256)]
        );
        assert_eq!(
            Mode::Default
                .settings()
                .scales
                .iter()
                .map(|scale| (scale.row_count, scale.query_amplification))
                .collect::<Vec<_>>(),
            [(1_000, 256), (10_000, 256), (50_000, 256)]
        );
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
