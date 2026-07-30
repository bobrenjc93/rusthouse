use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy)]
pub struct RatioObservation<'a> {
    pub profile: &'a str,
    pub seed: u64,
    pub family: &'a str,
    pub workload: &'a str,
    pub scale: usize,
    pub ratio: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct WorkloadDimension<'a> {
    pub profile: &'a str,
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

/// Aggregate capped ClickHouse/RustHouse ratios in log space.
///
/// Workloads are equal within each family/scale cell, followed by equal scale,
/// family, seed, and profile weight. The complete expected matrix is validated
/// before aggregation so missing or duplicated cases cannot silently reweight
/// the score.
pub fn parity_score(
    observations: &[RatioObservation<'_>],
    expected_profiles: &[&str],
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

    let expected_profiles = unique_expected(expected_profiles, "profile")?;
    let expected_seeds = unique_expected(expected_seeds, "seed")?;
    let expected_scales = unique_expected(expected_scales, "scale")?;
    let expected_workloads = expected_workload_matrix(expected_workloads, &expected_profiles)?;

    type WorkloadLogs<'a> = BTreeMap<&'a str, f64>;
    type ScaleLogs<'a> = BTreeMap<usize, WorkloadLogs<'a>>;
    type FamilyLogs<'a> = BTreeMap<&'a str, ScaleLogs<'a>>;
    type SeedLogs<'a> = BTreeMap<u64, FamilyLogs<'a>>;
    let mut grouped = BTreeMap::<&str, SeedLogs<'_>>::new();

    for observation in observations {
        if !expected_profiles.contains(observation.profile) {
            return Err(format!(
                "unexpected profile {:?} in benchmark observations",
                observation.profile
            ));
        }
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
            .entry(observation.profile)
            .or_default()
            .entry(observation.seed)
            .or_default()
            .entry(observation.family)
            .or_default()
            .entry(observation.scale)
            .or_default()
            .insert(
                observation.workload,
                observation.ratio.clamp(0.01, 1.0).ln(),
            );
        if previous.is_some() {
            return Err(format!(
                "duplicate benchmark observation for profile {:?}, seed {}, family {:?}, workload {:?}, scale {}",
                observation.profile,
                observation.seed,
                observation.family,
                observation.workload,
                observation.scale
            ));
        }
    }

    let actual_profiles = grouped.keys().copied().collect::<BTreeSet<_>>();
    if actual_profiles != expected_profiles {
        return Err(format!(
            "incomplete profile coverage: expected {expected_profiles:?}, got {actual_profiles:?}"
        ));
    }

    let mut profile_logs = Vec::with_capacity(grouped.len());
    for (profile, seeds) in &grouped {
        let actual_seeds = seeds.keys().copied().collect::<BTreeSet<_>>();
        if actual_seeds != expected_seeds {
            return Err(format!(
                "incomplete seed coverage for profile {profile:?}: expected {expected_seeds:?}, got {actual_seeds:?}"
            ));
        }

        let profile_workloads = &expected_workloads[profile];
        let expected_families = profile_workloads.keys().copied().collect::<BTreeSet<_>>();
        let mut seed_logs = Vec::with_capacity(seeds.len());
        for (seed, families) in seeds {
            let actual_families = families.keys().copied().collect::<BTreeSet<_>>();
            if actual_families != expected_families {
                return Err(format!(
                    "incomplete family coverage for profile {profile:?}, seed {seed}: expected {expected_families:?}, got {actual_families:?}"
                ));
            }

            let mut family_logs = Vec::with_capacity(families.len());
            for (family, scales) in families {
                let actual_scales = scales.keys().copied().collect::<BTreeSet<_>>();
                if actual_scales != expected_scales {
                    return Err(format!(
                        "incomplete scale coverage for profile {profile:?}, seed {seed}, family {family:?}: expected {expected_scales:?}, got {actual_scales:?}"
                    ));
                }

                let expected_cases = &profile_workloads[family];
                let mut scale_logs = Vec::with_capacity(scales.len());
                for (scale, workloads) in scales {
                    let actual_cases = workloads.keys().copied().collect::<BTreeSet<_>>();
                    if actual_cases != *expected_cases {
                        return Err(format!(
                            "incomplete workload coverage for profile {profile:?}, seed {seed}, family {family:?}, scale {scale}: expected {expected_cases:?}, got {actual_cases:?}"
                        ));
                    }
                    scale_logs.push(mean(workloads.values().copied()));
                }
                family_logs.push(mean(scale_logs));
            }
            seed_logs.push(mean(family_logs));
        }
        profile_logs.push(mean(seed_logs));
    }

    let score = (100.0 * mean(profile_logs).exp()).clamp(0.0, 100.0);
    let saturated_cases = observations
        .iter()
        .filter(|observation| observation.ratio >= 1.0)
        .count();
    Ok(ScoreBreakdown {
        score,
        saturated_cases,
    })
}

fn expected_workload_matrix<'a>(
    dimensions: &[WorkloadDimension<'a>],
    expected_profiles: &BTreeSet<&'a str>,
) -> Result<BTreeMap<&'a str, BTreeMap<&'a str, BTreeSet<&'a str>>>, String> {
    if dimensions.is_empty() {
        return Err("expected workload list must not be empty".to_owned());
    }
    let mut matrix = BTreeMap::<&str, BTreeMap<&str, BTreeSet<&str>>>::new();
    for dimension in dimensions {
        if !expected_profiles.contains(dimension.profile) {
            return Err(format!(
                "expected workload {:?} names unexpected profile {:?}",
                dimension.workload, dimension.profile
            ));
        }
        if !matrix
            .entry(dimension.profile)
            .or_default()
            .entry(dimension.family)
            .or_default()
            .insert(dimension.workload)
        {
            return Err(format!(
                "duplicate expected workload {:?} for profile {:?}, family {:?}",
                dimension.workload, dimension.profile, dimension.family
            ));
        }
    }

    let actual_profiles = matrix.keys().copied().collect::<BTreeSet<_>>();
    if actual_profiles != *expected_profiles {
        return Err(format!(
            "expected workload matrix has incomplete profiles: expected {expected_profiles:?}, got {actual_profiles:?}"
        ));
    }
    let mut family_sets = matrix
        .values()
        .map(|families| families.keys().copied().collect::<BTreeSet<_>>());
    let first = family_sets
        .next()
        .ok_or_else(|| "expected profile list must not be empty".to_owned())?;
    if family_sets.any(|families| families != first) {
        return Err(
            "expected workload matrix must use the same families for every profile".to_owned(),
        );
    }
    Ok(matrix)
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
    let mut count = 0_usize;
    let sum = values.into_iter().inspect(|_| count += 1).sum::<f64>();
    debug_assert!(count > 0);
    sum / count as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROFILES: [&str; 2] = ["numeric", "strings"];
    const SEEDS: [u64; 2] = [7, 8];
    const SCALES: [usize; 2] = [100, 1_000];
    const WORKLOADS: [WorkloadDimension<'static>; 4] = [
        WorkloadDimension {
            profile: "numeric",
            family: "scan",
            workload: "scan",
        },
        WorkloadDimension {
            profile: "numeric",
            family: "order",
            workload: "order",
        },
        WorkloadDimension {
            profile: "strings",
            family: "scan",
            workload: "scan",
        },
        WorkloadDimension {
            profile: "strings",
            family: "order",
            workload: "order",
        },
    ];

    #[test]
    fn score_math_has_documented_anchor_points() {
        assert!((single_score(1.0) - 100.0).abs() < 1e-12);
        assert!((single_score(0.1) - 10.0).abs() < 1e-12);
        assert!((single_score(100.0) - 100.0).abs() < 1e-12);
    }

    #[test]
    fn profiles_seeds_families_and_scales_receive_equal_weight() {
        let mut observations = complete_matrix(1.0);
        for observation in &mut observations {
            if observation.profile == "numeric"
                && observation.seed == 7
                && observation.family == "scan"
                && observation.scale == 100
            {
                observation.ratio = 0.01;
            }
        }

        let score = parity_score(&observations, &PROFILES, &SEEDS, &SCALES, &WORKLOADS)
            .expect("complete score")
            .score;
        assert!((score - 74.989_420_933).abs() < 1e-9);
    }

    #[test]
    fn duplicated_favorable_workload_is_rejected_instead_of_reweighting() {
        let mut observations = complete_matrix(1.0);
        observations.push(observations[0]);
        assert!(
            parity_score(&observations, &PROFILES, &SEEDS, &SCALES, &WORKLOADS)
                .expect_err("duplicate must fail")
                .contains("duplicate benchmark observation")
        );
    }

    #[test]
    fn missing_case_at_each_dimension_fails_closed() {
        let complete = complete_matrix(1.0);
        for needle in [("numeric", 7, "scan", 100), ("strings", 8, "order", 1_000)] {
            let observations = complete
                .iter()
                .copied()
                .filter(|observation| {
                    (
                        observation.profile,
                        observation.seed,
                        observation.family,
                        observation.scale,
                    ) != needle
                })
                .collect::<Vec<_>>();
            assert!(parity_score(&observations, &PROFILES, &SEEDS, &SCALES, &WORKLOADS).is_err());
        }

        assert!(parity_score(&complete, &["numeric"], &SEEDS, &SCALES, &WORKLOADS).is_err());
        assert!(parity_score(&complete, &PROFILES, &[7], &SCALES, &WORKLOADS).is_err());
        assert!(parity_score(&complete, &PROFILES, &SEEDS, &[100], &WORKLOADS).is_err());
    }

    #[test]
    fn all_capped_cases_are_reported_as_saturated() {
        let observations = complete_matrix(2.0);
        let breakdown =
            parity_score(&observations, &PROFILES, &SEEDS, &SCALES, &WORKLOADS).expect("score");
        assert_eq!(breakdown.saturated_cases, observations.len());
        assert_eq!(breakdown.score, 100.0);
    }

    #[test]
    fn median_handles_odd_and_even_sample_counts() {
        assert_eq!(median(&[9.0, 1.0, 3.0]).expect("median"), 3.0);
        assert_eq!(median(&[9.0, 1.0, 3.0, 5.0]).expect("median"), 4.0);
    }

    #[test]
    fn score_rejects_invalid_inputs_and_expected_dimensions() {
        assert!(parity_score(&[], &PROFILES, &SEEDS, &SCALES, &WORKLOADS).is_err());
        assert!(parity_score(&complete_matrix(1.0), &[], &SEEDS, &SCALES, &WORKLOADS).is_err());
        assert!(parity_score(&complete_matrix(1.0), &PROFILES, &[], &SCALES, &WORKLOADS).is_err());
        assert!(parity_score(&complete_matrix(1.0), &PROFILES, &SEEDS, &[], &WORKLOADS).is_err());
        assert!(single_score_result(0.0).is_err());
        assert!(single_score_result(f64::NAN).is_err());
    }

    fn single_score(ratio: f64) -> f64 {
        single_score_result(ratio).expect("score").score
    }

    fn single_score_result(ratio: f64) -> Result<ScoreBreakdown, String> {
        let observations = [RatioObservation {
            profile: "profile",
            seed: 1,
            family: "family",
            workload: "case",
            scale: 1,
            ratio,
        }];
        parity_score(
            &observations,
            &["profile"],
            &[1],
            &[1],
            &[WorkloadDimension {
                profile: "profile",
                family: "family",
                workload: "case",
            }],
        )
    }

    fn complete_matrix(ratio: f64) -> Vec<RatioObservation<'static>> {
        let mut observations = Vec::new();
        for profile in PROFILES {
            for seed in SEEDS {
                for family in ["scan", "order"] {
                    for scale in SCALES {
                        observations.push(RatioObservation {
                            profile,
                            seed,
                            family,
                            workload: family,
                            scale,
                            ratio,
                        });
                    }
                }
            }
        }
        observations
    }
}
