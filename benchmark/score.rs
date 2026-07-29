use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy)]
pub struct RatioObservation<'a> {
    pub family: &'a str,
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
/// family/scale, scales within a family, and finally families receive equal
/// log-space weight at each level.
pub fn parity_score(observations: &[RatioObservation<'_>]) -> Result<ScoreBreakdown, String> {
    if observations.is_empty() {
        return Err("cannot score an empty benchmark".to_owned());
    }
    if observations
        .iter()
        .any(|observation| !observation.ratio.is_finite() || observation.ratio <= 0.0)
    {
        return Err("benchmark ratios must be finite and positive".to_owned());
    }

    let mut grouped = BTreeMap::<&str, BTreeMap<usize, Vec<f64>>>::new();
    for observation in observations {
        grouped
            .entry(observation.family)
            .or_default()
            .entry(observation.scale)
            .or_default()
            .push(observation.ratio.clamp(0.01, 1.0).ln());
    }

    let family_logs = grouped
        .values()
        .map(|scales| {
            let scale_logs = scales
                .values()
                .map(|case_logs| mean(case_logs))
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

fn mean(values: &[f64]) -> f64 {
    values.iter().sum::<f64>() / values.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_math_has_documented_anchor_points() {
        assert!((score(&[("family", 1, 1.0)]) - 100.0).abs() < 1e-12);
        assert!((score(&[("family", 1, 0.1)]) - 10.0).abs() < 1e-12);
        assert!((score(&[("slow", 1, 0.1), ("fast", 1, 10.0)]) - 31.622_776).abs() < 1e-5);
    }

    #[test]
    fn repeated_favorable_workloads_cannot_dominate_other_families() {
        let baseline = score(&[("slow", 1, 0.1), ("fast", 1, 1.0)]);
        let mut duplicated = vec![("slow", 1, 0.1)];
        duplicated.extend(std::iter::repeat_n(("fast", 1, 100.0), 100));
        assert!((score(&duplicated) - baseline).abs() < 1e-12);
    }

    #[test]
    fn all_capped_cases_are_reported_as_saturated() {
        let observations = observations(&[("a", 1, 2.0), ("b", 1, 50.0)]);
        let breakdown = parity_score(&observations).expect("score");
        assert_eq!(breakdown.saturated_cases, observations.len());
        assert_eq!(breakdown.score, 100.0);
    }

    #[test]
    fn equal_weighting_applies_to_scales_and_families() {
        let value = score(&[("a", 1, 0.01), ("a", 2, 1.0), ("b", 1, 1.0), ("b", 2, 1.0)]);
        assert!((value - 31.622_776).abs() < 1e-5);
    }

    #[test]
    fn median_handles_odd_and_even_sample_counts() {
        assert_eq!(median(&[9.0, 1.0, 3.0]).expect("median"), 3.0);
        assert_eq!(median(&[9.0, 1.0, 3.0, 5.0]).expect("median"), 4.0);
    }

    #[test]
    fn score_rejects_invalid_inputs() {
        assert!(parity_score(&[]).is_err());
        assert!(parity_score(&observations(&[("a", 1, 0.0)])).is_err());
        assert!(parity_score(&observations(&[("a", 1, f64::NAN)])).is_err());
    }

    fn score(values: &[(&'static str, usize, f64)]) -> f64 {
        parity_score(&observations(values)).expect("score").score
    }

    fn observations(values: &[(&'static str, usize, f64)]) -> Vec<RatioObservation<'static>> {
        values
            .iter()
            .map(|(family, scale, ratio)| RatioObservation {
                family,
                scale: *scale,
                ratio: *ratio,
            })
            .collect()
    }
}
