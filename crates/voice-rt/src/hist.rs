use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Duration;

/// Upper bucket bounds in microseconds. Chosen around the 80 ms frame budget.
const BOUNDS_US: [u64; 14] =
    [100, 250, 500, 1_000, 2_500, 5_000, 10_000, 20_000, 40_000, 80_000, 160_000, 320_000, 640_000, 1_280_000];

/// Fixed-bucket latency histogram. Recording is wait-free and never allocates, so it is safe on
/// the audio path.
#[derive(Default)]
pub struct Histogram {
    buckets: [AtomicU64; BOUNDS_US.len()],
    overflow: AtomicU64,
    sum_us: AtomicU64,
    count: AtomicU64,
}

impl Histogram {
    pub const fn new() -> Self {
        Self {
            buckets: [const { AtomicU64::new(0) }; BOUNDS_US.len()],
            overflow: AtomicU64::new(0),
            sum_us: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    pub fn record(&self, d: Duration) {
        let us = d.as_micros().min(u64::MAX as u128) as u64;
        match BOUNDS_US.iter().position(|&b| us <= b) {
            Some(i) => self.buckets[i].fetch_add(1, Relaxed),
            None => self.overflow.fetch_add(1, Relaxed),
        };
        self.sum_us.fetch_add(us, Relaxed);
        self.count.fetch_add(1, Relaxed);
    }

    pub fn count(&self) -> u64 {
        self.count.load(Relaxed)
    }

    pub fn mean(&self) -> Option<Duration> {
        let n = self.count();
        (n > 0).then(|| Duration::from_micros(self.sum_us.load(Relaxed) / n))
    }

    /// Upper bound of the bucket holding quantile `q`. `None` when empty or when the quantile
    /// falls past the last bound.
    pub fn quantile_upper_bound(&self, q: f64) -> Option<Duration> {
        let n = self.count();
        if n == 0 {
            return None;
        }
        let rank = ((n as f64) * q).ceil().max(1.0) as u64;
        let mut seen = 0;
        for (bucket, bound) in self.buckets.iter().zip(BOUNDS_US) {
            seen += bucket.load(Relaxed);
            if seen >= rank {
                return Some(Duration::from_micros(bound));
            }
        }
        None
    }

    /// Appends the histogram in Prometheus text exposition format, in seconds.
    pub fn render(&self, name: &str, help: &str, out: &mut String) {
        let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} histogram");
        let mut cumulative = 0;
        for (bucket, bound) in self.buckets.iter().zip(BOUNDS_US) {
            cumulative += bucket.load(Relaxed);
            let _ = writeln!(out, "{name}_bucket{{le=\"{}\"}} {cumulative}", bound as f64 / 1e6);
        }
        cumulative += self.overflow.load(Relaxed);
        let _ = writeln!(out, "{name}_bucket{{le=\"+Inf\"}} {cumulative}");
        let _ = writeln!(out, "{name}_sum {}", self.sum_us.load(Relaxed) as f64 / 1e6);
        let _ = writeln!(out, "{name}_count {cumulative}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantiles_and_render() {
        let h = Histogram::new();
        assert_eq!(h.quantile_upper_bound(0.5), None);
        for _ in 0..99 {
            h.record(Duration::from_micros(900));
        }
        h.record(Duration::from_millis(30));
        assert_eq!(h.quantile_upper_bound(0.5), Some(Duration::from_millis(1)));
        assert_eq!(h.quantile_upper_bound(1.0), Some(Duration::from_millis(40)));
        let mut out = String::new();
        h.render("x_seconds", "help", &mut out);
        assert!(out.contains("x_seconds_bucket{le=\"0.001\"} 99"));
        assert!(out.contains("x_seconds_count 100"));
    }
}
