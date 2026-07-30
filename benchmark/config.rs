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
                row_counts: vec![256, 2_048],
                seed_count: 1,
                warmups: 1,
                samples: 3,
                query_amplification: 256,
                end_to_end_samples: 3,
            },
            Self::Default => BenchmarkSettings {
                row_counts: vec![1_000, 10_000, 50_000],
                seed_count: 3,
                warmups: 2,
                samples: 7,
                query_amplification: 256,
                end_to_end_samples: 3,
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct BenchmarkSettings {
    pub row_counts: Vec<usize>,
    pub seed_count: usize,
    pub warmups: usize,
    pub samples: usize,
    pub query_amplification: usize,
    pub end_to_end_samples: usize,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub mode: Mode,
    pub seed: u64,
    pub seed_count: usize,
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
    let mut seed_count = None;
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
            "--seed-count" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "--seed-count requires a positive integer".to_owned())?;
                seed_count = Some(parse_seed_count(&value)?);
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
            _ if argument.starts_with("--seed-count=") => {
                seed_count = Some(parse_seed_count(&argument["--seed-count=".len()..])?);
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

    let seed_count = seed_count.unwrap_or_else(|| mode.settings().seed_count);
    let clickhouse = clickhouse.ok_or_else(|| {
        "ClickHouse path is required; use --clickhouse PATH or RUSTHOUSE_CLICKHOUSE_BIN".to_owned()
    })?;
    Ok(ParseResult::Run(Config {
        mode,
        seed,
        seed_count,
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

fn parse_seed_count(value: &str) -> Result<usize, String> {
    let count = value
        .parse::<usize>()
        .map_err(|_| format!("invalid seed count {value:?}; expected a positive integer"))?;
    if count == 0 {
        return Err("seed count must be positive".to_owned());
    }
    Ok(count)
}

pub fn derive_seeds(root_seed: u64, count: usize) -> Result<Vec<u64>, String> {
    if count == 0 {
        return Err("seed count must be positive".to_owned());
    }

    let mut seeds = Vec::with_capacity(count);
    let mut state = root_seed;
    seeds.push(root_seed);
    for _ in 1..count {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        seeds.push(value ^ (value >> 31));
    }
    Ok(seeds)
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
                "--seed-count=5",
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
        assert_eq!(config.seed_count, 5);
        assert_eq!(config.clickhouse, PathBuf::from("/command/clickhouse"));
        assert_eq!(config.rusthouse, PathBuf::from("/command/rusthouse"));
        assert_eq!(config.details, Some(PathBuf::from("details.json")));
    }

    #[test]
    fn modes_use_multiple_row_counts_and_repeated_samples() {
        for mode in [Mode::Quick, Mode::Default] {
            let settings = mode.settings();
            assert!(settings.row_counts.len() >= 2);
            assert!(settings.seed_count >= 1);
            assert!(settings.warmups >= 1);
            assert!(settings.samples >= 3);
            assert!(settings.query_amplification > 1);
            assert!(settings.end_to_end_samples >= 3);
        }
    }

    #[test]
    fn clickhouse_path_is_required() {
        let error = match parse(std::iter::empty(), None, None, PathBuf::from("rusthouse")) {
            Ok(_) => panic!("missing ClickHouse path should fail"),
            Err(error) => error,
        };
        assert!(error.contains("RUSTHOUSE_CLICKHOUSE_BIN"));
    }

    #[test]
    fn modes_choose_seed_count_defaults_and_allow_an_override() {
        let ParseResult::Run(quick) = parse(
            ["--quick", "--clickhouse=/clickhouse"]
                .into_iter()
                .map(str::to_owned),
            None,
            None,
            PathBuf::from("rusthouse"),
        )
        .expect("quick configuration") else {
            panic!("expected run");
        };
        let ParseResult::Run(default) = parse(
            ["--clickhouse=/clickhouse"].into_iter().map(str::to_owned),
            None,
            None,
            PathBuf::from("rusthouse"),
        )
        .expect("default configuration") else {
            panic!("expected run");
        };

        assert_eq!(quick.seed_count, 1);
        assert_eq!(default.seed_count, 3);
        assert!(
            parse(
                ["--seed-count=0", "--clickhouse=/clickhouse"]
                    .into_iter()
                    .map(str::to_owned),
                None,
                None,
                PathBuf::from("rusthouse"),
            )
            .is_err()
        );
    }

    #[test]
    fn deterministic_seed_derivation_preserves_root_and_splitmix_sequence() {
        assert_eq!(
            derive_seeds(0, 4).expect("derived seeds"),
            [
                0,
                0xe220_a839_7b1d_cdaf,
                0x6e78_9e6a_a1b9_65f4,
                0x06c4_5d18_8009_454f,
            ]
        );
        assert_eq!(derive_seeds(u64::MAX, 1).expect("one seed"), [u64::MAX]);
        assert!(derive_seeds(7, 0).is_err());
    }
}
