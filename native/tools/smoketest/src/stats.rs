//! Timing aggregation: nearest-rank percentiles over per-stage samples.

/// Summary of one stage's samples, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Summary {
    pub count: usize,
    pub avg: f64,
    pub p50: f64,
    pub p95: f64,
    pub max: f64,
}

impl Summary {
    /// `None` when there are no samples: a percentile of nothing is not 0 ms,
    /// and reporting it as such would let a dead stage pass the budget.
    pub fn of(samples: &[f64]) -> Option<Self> {
        if samples.is_empty() {
            return None;
        }
        let mut sorted = samples.to_vec();
        // total_cmp: a NaN sorts last instead of panicking the comparison.
        sorted.sort_by(f64::total_cmp);
        Some(Self {
            count: sorted.len(),
            avg: sorted.iter().sum::<f64>() / sorted.len() as f64,
            p50: percentile(&sorted, 50),
            p95: percentile(&sorted, 95),
            max: sorted[sorted.len() - 1],
        })
    }
}

/// Nearest-rank percentile of an ascending, non-empty slice. Nearest-rank
/// (rather than interpolation) so the reported value is a latency that was
/// actually observed.
fn percentile(sorted: &[f64], pct: usize) -> f64 {
    let rank = (pct * sorted.len()).div_ceil(100).max(1);
    sorted[rank - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_samples_is_no_summary() {
        assert_eq!(Summary::of(&[]), None);
    }

    #[test]
    fn single_sample_fills_every_field() {
        let s = Summary::of(&[2.5]).expect("one sample");
        assert_eq!(
            s,
            Summary {
                count: 1,
                avg: 2.5,
                p50: 2.5,
                p95: 2.5,
                max: 2.5
            }
        );
    }

    #[test]
    fn percentiles_are_nearest_rank_over_unsorted_input() {
        let mut v: Vec<f64> = (1..=100).map(f64::from).collect();
        v.reverse();
        v.swap(3, 77);
        let s = Summary::of(&v).expect("samples");
        assert_eq!(s.count, 100);
        assert_eq!(s.p50, 50.0);
        assert_eq!(s.p95, 95.0);
        assert_eq!(s.max, 100.0);
        assert_eq!(s.avg, 50.5);
    }

    #[test]
    fn a_single_outlier_moves_max_but_not_p95_of_a_hundred() {
        let mut v = vec![1.0; 99];
        v.push(500.0);
        let s = Summary::of(&v).expect("samples");
        assert_eq!(s.p95, 1.0);
        assert_eq!(s.max, 500.0);
    }

    #[test]
    fn small_sets_round_the_rank_up() {
        let s = Summary::of(&[10.0, 20.0, 30.0]).expect("samples");
        assert_eq!(s.p50, 20.0);
        assert_eq!(s.p95, 30.0);
    }
}
