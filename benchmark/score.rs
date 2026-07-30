use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy)]
pub struct RatioObservation<'a> {
    pub family: &'a str,
    pub workload: &'a str,
    pub scale: usize,
    pub seed: u64,
    pub ratio: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct WorkloadDimension<'a> {
    pub family: &'a str,
    pub workload: &'a str,
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
/// Case ratios are capped at parity and floored at 0.01. Workloads receive
/// equal log-space weight within a seed, seeds within a family/scale, scales
/// within a family, and finally families. The expected seed/scale matrix must
/// be complete, with the same workloads for every seed in a family.
pub fn parity_score(
    observations: &[RatioObservation<'_>],
    expected_seeds: &[u64],
    expected_scales: &[usize],
    expected_workloads: &[WorkloadDimension<'_>],
) -> Result<ScoreBreakdown, String> {
    if observations.is_empty() {
        return Err("cannot score an empty benchmark".to_owned());
    }
    if observations
        .iter()
        .any(|observation| !observation.ratio.is_finite() || observation.ratio <= 0.0)
    {
        return Err("benchmark ratios must be finite and positive".to_owned());
    }

    let expected_seeds = unique_expected(expected_seeds, "seed")?;
    let expected_scales = unique_expected(expected_scales, "scale")?;
    let mut expected_families = BTreeMap::<&str, BTreeSet<&str>>::new();
    for dimension in expected_workloads {
        if !expected_families
            .entry(dimension.family)
            .or_default()
            .insert(dimension.workload)
        {
            return Err(format!(
                "duplicate expected workload {:?} in family {:?}",
                dimension.workload, dimension.family
            ));
        }
    }
    if expected_families.is_empty() {
        return Err("expected workload list must not be empty".to_owned());
    }
    let mut grouped = BTreeMap::<&str, BTreeMap<usize, BTreeMap<u64, BTreeMap<&str, f64>>>>::new();
    for observation in observations {
        if !expected_seeds.contains(&observation.seed) {
            return Err(format!(
                "unexpected seed {} in benchmark observations",
                observation.seed
            ));
        }
        if !expected_scales.contains(&observation.scale) {
            return Err(format!(
                "unexpected scale {} in benchmark observations",
                observation.scale
            ));
        }
        let previous = grouped
            .entry(observation.family)
            .or_default()
            .entry(observation.scale)
            .or_default()
            .entry(observation.seed)
            .or_default()
            .insert(
                observation.workload,
                observation.ratio.clamp(0.01, 1.0).ln(),
            );
        if previous.is_some() {
            return Err(format!(
                "duplicate benchmark observation for family {:?}, workload {:?}, scale {}, seed {}",
                observation.family, observation.workload, observation.scale, observation.seed
            ));
        }
    }

    let actual_families = grouped.keys().copied().collect::<BTreeSet<_>>();
    let expected_family_names = expected_families.keys().copied().collect::<BTreeSet<_>>();
    if actual_families != expected_family_names {
        return Err(format!(
            "incomplete family coverage: expected {expected_family_names:?}, got {actual_families:?}"
        ));
    }

    let mut family_logs = Vec::with_capacity(grouped.len());
    for (family, scales) in &grouped {
        let actual_scales = scales.keys().copied().collect::<BTreeSet<_>>();
        if actual_scales != expected_scales {
            return Err(format!(
                "incomplete scale coverage for family {family:?}: expected {expected_scales:?}, got {actual_scales:?}"
            ));
        }
        let expected_workloads = &expected_families[family];

        let mut scale_logs = Vec::with_capacity(scales.len());
        for (scale, seeds) in scales {
            let actual_seeds = seeds.keys().copied().collect::<BTreeSet<_>>();
            if actual_seeds != expected_seeds {
                return Err(format!(
                    "incomplete seed coverage for family {family:?}, scale {scale}: expected {expected_seeds:?}, got {actual_seeds:?}"
                ));
            }

            let mut seed_logs = Vec::with_capacity(seeds.len());
            for (seed, workloads) in seeds {
                let actual_workloads = workloads.keys().copied().collect::<BTreeSet<_>>();
                if actual_workloads != *expected_workloads {
                    return Err(format!(
                        "incomplete workload coverage for family {family:?}, scale {scale}, seed {seed}: expected {expected_workloads:?}, got {actual_workloads:?}"
                    ));
                }
                seed_logs.push(mean(workloads.values().copied()));
            }
            scale_logs.push(mean(seed_logs));
        }
        family_logs.push(mean(scale_logs));
    }

    let score = (100.0 * mean(family_logs).exp()).clamp(0.0, 100.0);
    let saturated_cases = observations
        .iter()
        .filter(|observation| observation.ratio >= 1.0)
        .count();
    Ok(ScoreBreakdown {
        score,
        saturated_cases,
    })
}

fn unique_expected<T>(values: &[T], name: &str) -> Result<BTreeSet<T>, String>
where
    T: Copy + Ord,
{
    if values.is_empty() {
        return Err(format!("expected {name} list must not be empty"));
    }
    let unique = values.iter().copied().collect::<BTreeSet<_>>();
    if unique.len() != values.len() {
        return Err(format!("expected {name} list contains duplicates"));
    }
    Ok(unique)
}

fn mean(values: impl IntoIterator<Item = f64>) -> f64 {
    let (sum, count) = values
        .into_iter()
        .fold((0.0, 0_usize), |(sum, count), value| {
            (sum + value, count + 1)
        });
    sum / count as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    type Observation = (&'static str, &'static str, usize, u64, f64);

    #[test]
    fn score_math_has_documented_anchor_points() {
        assert!((score(&[("family", "case", 1, 7, 1.0)], &[7], &[1]) - 100.0).abs() < 1e-12);
        assert!((score(&[("family", "case", 1, 7, 0.1)], &[7], &[1]) - 10.0).abs() < 1e-12);
        assert!(
            (score(
                &[
                    ("slow", "slow_case", 1, 7, 0.1),
                    ("fast", "fast_case", 1, 7, 10.0),
                ],
                &[7],
                &[1],
            ) - 31.622_776)
                .abs()
                < 1e-5
        );
    }

    #[test]
    fn favorable_workload_count_cannot_dominate_other_families() {
        let baseline = score(
            &[
                ("slow", "slow_case", 1, 7, 0.1),
                ("fast", "fast_a", 1, 7, 1.0),
            ],
            &[7],
            &[1],
        );
        let expanded = score(
            &[
                ("slow", "slow_case", 1, 7, 0.1),
                ("fast", "fast_a", 1, 7, 1.0),
                ("fast", "fast_b", 1, 7, 100.0),
                ("fast", "fast_c", 1, 7, 50.0),
            ],
            &[7],
            &[1],
        );
        assert!((expanded - baseline).abs() < 1e-12);
    }

    #[test]
    fn all_capped_cases_are_reported_as_saturated() {
        let values = [("a", "a_case", 1, 7, 2.0), ("b", "b_case", 1, 7, 50.0)];
        let observations = observations(&values);
        let breakdown =
            parity_score(&observations, &[7], &[1], &workload_dimensions(&values)).expect("score");
        assert_eq!(breakdown.saturated_cases, observations.len());
        assert_eq!(breakdown.score, 100.0);
    }

    #[test]
    fn equal_weighting_applies_to_seeds_scales_and_families() {
        let values = [
            ("a", "a_case", 1, 10, 0.01),
            ("a", "a_case", 1, 20, 1.0),
            ("a", "a_case", 2, 10, 1.0),
            ("a", "a_case", 2, 20, 1.0),
            ("b", "b_case", 1, 10, 1.0),
            ("b", "b_case", 1, 20, 1.0),
            ("b", "b_case", 2, 10, 1.0),
            ("b", "b_case", 2, 20, 1.0),
        ];
        let value = score(&values, &[10, 20], &[1, 2]);
        assert!((value - 56.234_132).abs() < 1e-5);
    }

    #[test]
    fn complete_matrix_is_order_invariant() {
        let mut values = vec![
            ("a", "first", 1, 10, 0.2),
            ("a", "second", 1, 10, 0.8),
            ("a", "first", 1, 20, 0.4),
            ("a", "second", 1, 20, 1.0),
        ];
        let forward = score(&values, &[10, 20], &[1]);
        values.reverse();
        let reverse = score(&values, &[20, 10], &[1]);
        assert!((forward - reverse).abs() < 1e-12);
    }

    #[test]
    fn incomplete_seed_or_workload_coverage_fails_closed() {
        let missing_seed = [("a", "case", 1, 10, 1.0)];
        assert!(score_result(&missing_seed, &[10, 20], &[1]).is_err());

        let missing_workload = [
            ("a", "first", 1, 10, 1.0),
            ("a", "second", 1, 10, 1.0),
            ("a", "first", 1, 20, 1.0),
        ];
        assert!(score_result(&missing_workload, &[10, 20], &[1]).is_err());

        let observations = observations(&[("a", "case", 1, 10, 1.0)]);
        let expected = [
            WorkloadDimension {
                family: "a",
                workload: "case",
            },
            WorkloadDimension {
                family: "missing_family",
                workload: "missing_case",
            },
        ];
        assert!(parity_score(&observations, &[10], &[1], &expected).is_err());
    }

    #[test]
    fn duplicate_and_unexpected_cells_fail_closed() {
        let duplicate = [("a", "case", 1, 10, 1.0), ("a", "case", 1, 10, 1.0)];
        assert!(score_result(&duplicate, &[10], &[1]).is_err());

        let unexpected = [("a", "case", 1, 20, 1.0)];
        assert!(score_result(&unexpected, &[10], &[1]).is_err());
    }

    #[test]
    fn median_handles_odd_and_even_sample_counts() {
        assert_eq!(median(&[9.0, 1.0, 3.0]).expect("median"), 3.0);
        assert_eq!(median(&[9.0, 1.0, 3.0, 5.0]).expect("median"), 4.0);
    }

    #[test]
    fn score_rejects_invalid_inputs() {
        assert!(parity_score(&[], &[7], &[1], &[]).is_err());
        assert!(score_result(&[("a", "case", 1, 7, 0.0)], &[7], &[1]).is_err());
        assert!(score_result(&[("a", "case", 1, 7, f64::NAN)], &[7], &[1]).is_err());
        assert!(score_result(&[("a", "case", 1, 7, 1.0)], &[], &[1]).is_err());
    }

    fn score(values: &[Observation], seeds: &[u64], scales: &[usize]) -> f64 {
        score_result(values, seeds, scales).expect("score").score
    }

    fn score_result(
        values: &[Observation],
        seeds: &[u64],
        scales: &[usize],
    ) -> Result<ScoreBreakdown, String> {
        parity_score(
            &observations(values),
            seeds,
            scales,
            &workload_dimensions(values),
        )
    }

    fn observations(values: &[Observation]) -> Vec<RatioObservation<'static>> {
        values
            .iter()
            .map(|(family, workload, scale, seed, ratio)| RatioObservation {
                family,
                workload,
                scale: *scale,
                seed: *seed,
                ratio: *ratio,
            })
            .collect()
    }

    fn workload_dimensions(values: &[Observation]) -> Vec<WorkloadDimension<'static>> {
        values
            .iter()
            .map(|(family, workload, ..)| (*family, *workload))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|(family, workload)| WorkloadDimension { family, workload })
            .collect()
    }
}
