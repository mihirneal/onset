//! Small statistics helpers shared by the timing code.

/// The middle value (or the mean of the two middle values). `None` if empty.
pub fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let mid = sorted.len() / 2;
    Some(if sorted.len().is_multiple_of(2) {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[mid]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_of_nothing_is_none() {
        assert_eq!(median(&[]), None);
    }

    #[test]
    fn median_of_odd_and_even_lengths() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
    }
}
