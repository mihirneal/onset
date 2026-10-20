//! The few statistics the timing check needs, written out by hand.
//!
//! They follow numpy's definitions (`std` with `ddof`, percentiles with
//! linear interpolation) so the numbers match what an analysis in numpy
//! would report.

/// Mean; NaN for an empty slice.
pub fn mean(x: &[f64]) -> f64 {
    if x.is_empty() {
        return f64::NAN;
    }
    x.iter().sum::<f64>() / x.len() as f64
}

/// Standard deviation dividing by `n - ddof` (numpy's `std`): `ddof = 1` is
/// the sample SD, `ddof = 0` the population SD. NaN if `n <= ddof`.
pub fn std(x: &[f64], ddof: usize) -> f64 {
    if x.len() <= ddof {
        return f64::NAN;
    }
    let m = mean(x);
    (x.iter().map(|v| (v - m).powi(2)).sum::<f64>() / (x.len() - ddof) as f64).sqrt()
}

/// Percentile `p` (0-100), interpolating linearly between neighbouring
/// values (numpy's default). NaN for an empty slice.
pub fn percentile(x: &[f64], p: f64) -> f64 {
    if x.is_empty() {
        return f64::NAN;
    }
    let mut v = x.to_vec();
    v.sort_by(f64::total_cmp);
    let rank = p / 100.0 * (v.len() - 1) as f64;
    let (lo, hi) = (rank.floor() as usize, rank.ceil() as usize);
    v[lo] + (v[hi] - v[lo]) * (rank - lo as f64)
}

pub fn median(x: &[f64]) -> f64 {
    percentile(x, 50.0)
}

/// The values that aren't NaN (what numpy's `nan*` functions use).
pub fn not_nan(x: &[f64]) -> Vec<f64> {
    x.iter().copied().filter(|v| !v.is_nan()).collect()
}

/// Largest minus smallest value; 0 for an empty slice.
pub fn range(x: &[f64]) -> f64 {
    let max = x.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let min = x.iter().copied().fold(f64::INFINITY, f64::min);
    if x.is_empty() { 0.0 } else { max - min }
}

/// Least-squares line `y = a * x + b`, centred for numerical stability.
/// Returns `(a, b)`; the slope is 1 if all x are equal.
pub fn fit_line(x: &[f64], y: &[f64]) -> (f64, f64) {
    let (xm, ym) = (mean(x), mean(y));
    let denom: f64 = x.iter().map(|v| (v - xm).powi(2)).sum();
    let a = if denom > 0.0 {
        x.iter().zip(y).map(|(xi, yi)| (xi - xm) * (yi - ym)).sum::<f64>() / denom
    } else {
        1.0
    };
    (a, ym - a * xm)
}

/// Summary of a set of latencies (ms).
#[derive(Debug, Clone)]
pub struct LatencyStats {
    pub n: usize,
    pub mean: f64,
    pub sd: f64,
    pub median: f64,
    pub min: f64,
    pub max: f64,
    pub p05: f64,
    pub p95: f64,
    /// Largest distance of one value from the mean.
    pub max_abs_dev: f64,
    /// Linear trend over the run: (ms per minute, SD around the trend).
    pub trend: Option<(f64, f64)>,
}

/// Stats of the finite values in `lat_ms`. With `times_s` (one per latency),
/// also the linear trend over time. `None` if no value is finite.
pub fn latency_stats(lat_ms: &[f64], times_s: Option<&[f64]>) -> Option<LatencyStats> {
    let keep: Vec<usize> = (0..lat_ms.len()).filter(|&i| lat_ms[i].is_finite()).collect();
    let lat: Vec<f64> = keep.iter().map(|&i| lat_ms[i]).collect();
    if lat.is_empty() {
        return None;
    }
    let m = mean(&lat);
    let mut trend = None;
    if let Some(times) = times_s {
        let t: Vec<f64> = keep.iter().map(|&i| times[i]).collect();
        if lat.len() > 2 && range(&t) > 0.0 {
            let (a, b) = fit_line(&t, &lat);
            let detrended: Vec<f64> = t.iter().zip(&lat).map(|(ti, li)| li - (a * ti + b)).collect();
            trend = Some((a * 60.0, std(&detrended, 1)));
        }
    }
    Some(LatencyStats {
        n: lat.len(),
        mean: m,
        sd: if lat.len() > 1 { std(&lat, 1) } else { 0.0 },
        median: median(&lat),
        min: lat.iter().copied().fold(f64::INFINITY, f64::min),
        max: lat.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        p05: percentile(&lat, 5.0),
        p95: percentile(&lat, 95.0),
        max_abs_dev: lat.iter().map(|v| (v - m).abs()).fold(0.0, f64::max),
        trend,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_numpy() {
        let x = [3.0, 1.0, 4.0, 1.0, 5.0, 9.0, 2.0, 6.0];
        assert!((std(&x, 1) - 2.748_376_143_938_713).abs() < 1e-12); // np.std(x, ddof=1)
        assert!((std(&x, 0) - 2.570_870_475_150_392).abs() < 1e-12);
        assert_eq!(median(&x), 3.5);
        assert!((percentile(&x, 5.0) - 1.0).abs() < 1e-12);
        assert!((percentile(&x, 95.0) - 7.95).abs() < 1e-12); // np.percentile(x, 95)
        assert!(median(&[]).is_nan());
        assert!(std(&[1.0], 1).is_nan());
    }

    #[test]
    fn fits_a_line() {
        let x = [0.0, 1.0, 2.0, 3.0];
        let y: Vec<f64> = x.iter().map(|v| 2.5 * v - 1.0).collect();
        let (a, b) = fit_line(&x, &y);
        assert!((a - 2.5).abs() < 1e-12 && (b + 1.0).abs() < 1e-12);
        assert_eq!(fit_line(&[1.0, 1.0], &[0.0, 2.0]), (1.0, 0.0));
    }

    #[test]
    fn latency_trend() {
        let t: Vec<f64> = (0..10).map(|i| i as f64 * 6.0).collect(); // one per 6 s
        let lat: Vec<f64> = t.iter().map(|ti| 12.0 + 0.1 * ti).collect(); // 6 ms/min
        let s = latency_stats(&lat, Some(&t)).unwrap();
        let (per_min, sd) = s.trend.unwrap();
        assert!((per_min - 6.0).abs() < 1e-9 && sd < 1e-9);
        assert!(latency_stats(&[f64::NAN], None).is_none());
    }
}
