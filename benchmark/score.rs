use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy)]
pub struct ExpectedCase<'a> {
    pub name: &'a str,
    pub family: &'a str,
}

#[derive(Debug, Clone, Copy)]
pub struct ScorePanel<'a> {
    pub seeds: &'a [u64],
    pub scales: &'a [usize],
    pub cases: &'a [ExpectedCase<'a>],
}

#[derive(Debug, Clone, Copy)]
pub struct RatioObservation<'a> {
    pub case: &'a str,
    pub family: &'a str,
    pub seed: u64,
    pub scale: usize,
    pub ratio: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct ScoreBreakdown {
    pub score: f64,
    pub saturated_cases: usize,
}

pub fn median(samples: &[f64]) -> Result<f64, String> {
    if samples.is_empty() {
        return Err("cannot compute a median without samples".to_owned());
    }
    if samples
        .iter()
        .any(|sample| !sample.is_finite() || *sample < 0.0)
    {
        return Err("timing samples must be finite and non-negative".to_owned());
    }

    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        Ok((sorted[middle - 1] + sorted[middle]) / 2.0)
    } else {
        Ok(sorted[middle])
    }
}

/// Aggregate ClickHouse/RustHouse ratios without allowing fast cases to
/// compensate for slow families.
///
/// Case ratios are capped at parity and floored at 0.01. Workloads within a
/// family/seed/scale, seeds within a family/scale, scales within a family, and
/// finally families receive equal log-space weight at each level.
pub fn parity_score(
    observations: &[RatioObservation<'_>],
    panel: ScorePanel<'_>,
) -> Result<ScoreBreakdown, String> {
    validate_panel(observations, panel)?;
    if observations
        .iter()
        .any(|observation| !observation.ratio.is_finite() || observation.ratio <= 0.0)
    {
        return Err("benchmark ratios must be finite and positive".to_owned());
    }

    let mut grouped = BTreeMap::<&str, BTreeMap<usize, BTreeMap<u64, Vec<f64>>>>::new();
    for observation in observations {
        grouped
            .entry(observation.family)
            .or_default()
            .entry(observation.scale)
            .or_default()
            .entry(observation.seed)
            .or_default()
            .push(observation.ratio.clamp(0.01, 1.0).ln());
    }

    let family_logs = grouped
        .values()
        .map(|scales| {
            let scale_logs = scales
                .values()
                .map(|seeds| {
                    let seed_logs = seeds
                        .values()
                        .map(|case_logs| mean(case_logs))
                        .collect::<Vec<_>>();
                    mean(&seed_logs)
                })
                .collect::<Vec<_>>();
            mean(&scale_logs)
        })
        .collect::<Vec<_>>();
    let score = (100.0 * mean(&family_logs).exp()).clamp(0.0, 100.0);
    let saturated_cases = observations
        .iter()
        .filter(|observation| observation.ratio >= 1.0)
        .count();
    Ok(ScoreBreakdown {
        score,
        saturated_cases,
    })
}

fn validate_panel(
    observations: &[RatioObservation<'_>],
    panel: ScorePanel<'_>,
) -> Result<(), String> {
    if panel.seeds.is_empty() || panel.scales.is_empty() || panel.cases.is_empty() {
        return Err("score panel seeds, scales, and cases must all be non-empty".to_owned());
    }

    let seeds = panel.seeds.iter().copied().collect::<BTreeSet<_>>();
    if seeds.len() != panel.seeds.len() {
        return Err("score panel contains duplicate seeds".to_owned());
    }
    let scales = panel.scales.iter().copied().collect::<BTreeSet<_>>();
    if scales.len() != panel.scales.len() {
        return Err("score panel contains duplicate scales".to_owned());
    }

    let mut expected_cases = BTreeMap::new();
    for expected in panel.cases {
        if expected.name.is_empty() || expected.family.is_empty() {
            return Err("score panel case names and families must be non-empty".to_owned());
        }
        if expected_cases
            .insert(expected.name, expected.family)
            .is_some()
        {
            return Err(format!(
                "score panel contains duplicate case {:?}",
                expected.name
            ));
        }
    }

    let expected_count = panel
        .seeds
        .len()
        .checked_mul(panel.scales.len())
        .and_then(|count| count.checked_mul(panel.cases.len()))
        .ok_or_else(|| "score panel case count overflowed".to_owned())?;
    if observations.len() != expected_count {
        return Err(format!(
            "incomplete score panel: expected {expected_count} seed/scale cases, got {}",
            observations.len()
        ));
    }

    let mut seen = BTreeSet::new();
    for observation in observations {
        let expected_family = expected_cases
            .get(observation.case)
            .ok_or_else(|| format!("unexpected benchmark case {:?}", observation.case))?;
        if observation.family != *expected_family {
            return Err(format!(
                "benchmark case {:?} used family {:?}; expected {:?}",
                observation.case, observation.family, expected_family
            ));
        }
        if !seeds.contains(&observation.seed) {
            return Err(format!("unexpected benchmark seed {}", observation.seed));
        }
        if !scales.contains(&observation.scale) {
            return Err(format!("unexpected benchmark scale {}", observation.scale));
        }
        if !seen.insert((observation.seed, observation.scale, observation.case)) {
            return Err(format!(
                "duplicate benchmark case {:?} for seed {} at scale {}",
                observation.case, observation.seed, observation.scale
            ));
        }
    }

    for seed in panel.seeds {
        for scale in panel.scales {
            for expected in panel.cases {
                if !seen.contains(&(*seed, *scale, expected.name)) {
                    return Err(format!(
                        "missing benchmark case {:?} for seed {seed} at scale {scale}",
                        expected.name
                    ));
                }
            }
        }
    }
    Ok(())
}

fn mean(values: &[f64]) -> f64 {
    values.iter().sum::<f64>() / values.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Mode;
    use crate::workload::workloads;

    const ONE_CASE: [ExpectedCase<'static>; 1] = [ExpectedCase {
        name: "case",
        family: "family",
    }];

    #[test]
    fn score_math_has_documented_anchor_points() {
        assert!((single_score(1.0) - 100.0).abs() < 1e-12);
        assert!((single_score(0.1) - 10.0).abs() < 1e-12);
    }

    #[test]
    fn repeated_favorable_workloads_cannot_dominate_other_families() {
        let cases = [
            ExpectedCase {
                name: "slow",
                family: "slow",
            },
            ExpectedCase {
                name: "fast_a",
                family: "fast",
            },
            ExpectedCase {
                name: "fast_b",
                family: "fast",
            },
        ];
        let observations = vec![
            observation("slow", "slow", 1, 1, 0.1),
            observation("fast_a", "fast", 1, 1, 1.0),
            observation("fast_b", "fast", 1, 1, 1.0),
        ];
        let value = parity_score(&observations, panel(&[1], &[1], &cases))
            .expect("score")
            .score;
        assert!((value - 31.622_776).abs() < 1e-5);
    }

    #[test]
    fn all_capped_cases_are_reported_as_saturated() {
        let observations = vec![observation("case", "family", 1, 1, 2.0)];
        let breakdown = parity_score(&observations, panel(&[1], &[1], &ONE_CASE)).expect("score");
        assert_eq!(breakdown.saturated_cases, observations.len());
        assert_eq!(breakdown.score, 100.0);
    }

    #[test]
    fn equal_weighting_applies_to_seeds_scales_and_families() {
        let cases = [
            ExpectedCase {
                name: "a",
                family: "a",
            },
            ExpectedCase {
                name: "b",
                family: "b",
            },
        ];
        let mut observations = complete_observations(&[1, 2], &[10, 20], &cases, 1.0);
        observations
            .iter_mut()
            .find(|value| value.case == "a" && value.seed == 1 && value.scale == 10)
            .expect("case")
            .ratio = 0.0001;

        let value = parity_score(&observations, panel(&[1, 2], &[10, 20], &cases))
            .expect("score")
            .score;
        assert!((value - 56.234_132).abs() < 1e-5);
    }

    #[test]
    fn every_seed_scale_case_is_required_and_any_missing_case_cannot_score() {
        let seeds = Mode::Default.benchmark_seeds(20_260_729);
        let scales = Mode::Default.settings().row_counts;
        let cases = workloads(1)
            .into_iter()
            .map(|workload| ExpectedCase {
                name: workload.name,
                family: workload.family.name(),
            })
            .collect::<Vec<_>>();
        let complete = complete_observations(&seeds, &scales, &cases, 0.5);
        assert_eq!(complete.len(), 96);
        parity_score(&complete, panel(&seeds, &scales, &cases)).expect("complete panel");

        for missing_index in 0..complete.len() {
            let mut incomplete = complete.clone();
            incomplete.remove(missing_index);
            let error = parity_score(&incomplete, panel(&seeds, &scales, &cases))
                .expect_err("missing cases must fail closed");
            assert!(error.contains("incomplete score panel"));
        }
    }

    #[test]
    fn duplicate_case_cannot_hide_a_missing_case() {
        let cases = [
            ExpectedCase {
                name: "scan",
                family: "scan",
            },
            ExpectedCase {
                name: "sort",
                family: "order",
            },
        ];
        let observations = vec![
            observation("scan", "scan", 1, 10, 0.5),
            observation("scan", "scan", 1, 10, 0.5),
        ];
        let error = parity_score(&observations, panel(&[1], &[10], &cases))
            .expect_err("duplicates must fail closed");
        assert!(error.contains("duplicate benchmark case"));
    }

    #[test]
    fn median_handles_odd_and_even_sample_counts() {
        assert_eq!(median(&[9.0, 1.0, 3.0]).expect("median"), 3.0);
        assert_eq!(median(&[9.0, 1.0, 3.0, 5.0]).expect("median"), 4.0);
    }

    #[test]
    fn score_rejects_invalid_inputs() {
        assert!(parity_score(&[], panel(&[1], &[1], &ONE_CASE)).is_err());
        assert!(
            parity_score(
                &[observation("case", "family", 1, 1, 0.0)],
                panel(&[1], &[1], &ONE_CASE)
            )
            .is_err()
        );
        assert!(
            parity_score(
                &[observation("case", "family", 1, 1, f64::NAN)],
                panel(&[1], &[1], &ONE_CASE)
            )
            .is_err()
        );
    }

    fn single_score(ratio: f64) -> f64 {
        parity_score(
            &[observation("case", "family", 1, 1, ratio)],
            panel(&[1], &[1], &ONE_CASE),
        )
        .expect("score")
        .score
    }

    fn observation(
        case: &'static str,
        family: &'static str,
        seed: u64,
        scale: usize,
        ratio: f64,
    ) -> RatioObservation<'static> {
        RatioObservation {
            case,
            family,
            seed,
            scale,
            ratio,
        }
    }

    fn panel<'a>(
        seeds: &'a [u64],
        scales: &'a [usize],
        cases: &'a [ExpectedCase<'a>],
    ) -> ScorePanel<'a> {
        ScorePanel {
            seeds,
            scales,
            cases,
        }
    }

    fn complete_observations(
        seeds: &[u64],
        scales: &[usize],
        cases: &[ExpectedCase<'static>],
        ratio: f64,
    ) -> Vec<RatioObservation<'static>> {
        let mut observations = Vec::new();
        for seed in seeds {
            for scale in scales {
                for case in cases {
                    observations.push(observation(case.name, case.family, *seed, *scale, ratio));
                }
            }
        }
        observations
    }
}
