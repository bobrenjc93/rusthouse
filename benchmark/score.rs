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

/// Aggregate ClickHouse/RustHouse speed ratios on a log scale.
///
/// Ratios are winsorized to [0.01, 100], then the outer 10 percent is
/// trimmed when at least ten cases exist. A ratio of one maps to parity
/// (100), and a ratio of 0.1 maps to approximately 10.
pub fn parity_score(ratios: &[f64]) -> Result<f64, String> {
    if ratios.is_empty() {
        return Err("cannot score an empty benchmark".to_owned());
    }
    if ratios
        .iter()
        .any(|ratio| !ratio.is_finite() || *ratio <= 0.0)
    {
        return Err("benchmark ratios must be finite and positive".to_owned());
    }

    let mut logs = ratios
        .iter()
        .map(|ratio| ratio.clamp(0.01, 100.0).ln())
        .collect::<Vec<_>>();
    logs.sort_by(f64::total_cmp);
    let trim = if logs.len() >= 10 { logs.len() / 10 } else { 0 };
    let retained = &logs[trim..logs.len() - trim];
    let log_mean = retained.iter().sum::<f64>() / retained.len() as f64;
    Ok((100.0 * log_mean.exp()).clamp(0.0, 100.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_math_has_documented_anchor_points() {
        assert!((parity_score(&[1.0]).expect("score") - 100.0).abs() < 1e-12);
        assert!((parity_score(&[0.1]).expect("score") - 10.0).abs() < 1e-12);
        assert!((parity_score(&[0.1, 10.0]).expect("score") - 100.0).abs() < 1e-12);
    }

    #[test]
    fn robust_score_trims_a_single_extreme_outlier() {
        let mut ratios = vec![1.0; 9];
        ratios.push(0.000_001);
        assert!((parity_score(&ratios).expect("score") - 100.0).abs() < 1e-12);
    }

    #[test]
    fn median_handles_odd_and_even_sample_counts() {
        assert_eq!(median(&[9.0, 1.0, 3.0]).expect("median"), 3.0);
        assert_eq!(median(&[9.0, 1.0, 3.0, 5.0]).expect("median"), 4.0);
    }

    #[test]
    fn score_rejects_invalid_inputs() {
        assert!(parity_score(&[]).is_err());
        assert!(parity_score(&[0.0]).is_err());
        assert!(parity_score(&[f64::NAN]).is_err());
    }
}
