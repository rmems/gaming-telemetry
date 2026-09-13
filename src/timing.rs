// SPDX-License-Identifier: MIT OR Apache-2.0

//! Timing-quality tracking for the poll loop.
//!
//! A nominal 5 ms stream does not necessarily behave like a 5 ms stream. Anything
//! replaying this data needs to know how well the requested cadence actually held,
//! so the collector measures the observed interval between samples and reports
//! percentiles alongside late/skipped counts in the session manifest.
//!
//! Intervals are measured on a **monotonic** clock (`Instant`), while row
//! timestamps come from the wall clock (`Utc::now()`). Those are different time
//! bases — a wall-clock step (NTP) moves row timestamps but not these intervals —
//! so the summary records which basis is which.

use serde::{Deserialize, Serialize};
use std::time::Instant;

/// Histogram resolution. 100 µs buckets are finer than any cadence this collector
/// polls at, so percentile error stays below the bucket width.
const BUCKET_US: u64 = 100;
/// Buckets cover 0..100 ms at 100 µs resolution.
const BUCKET_COUNT: usize = 1000;
/// Above 100 ms, retain millisecond-resolution buckets through ten seconds. That
/// keeps common storage/backpressure delays distinguishable without making
/// tracker memory depend on capture length.
const TAIL_BUCKET_US: u64 = 1_000;
const TAIL_BUCKET_COUNT: usize = 9_900;
const TAIL_CEILING_US: u64 = 10_000_000;
/// Longer gaps use exponentially wider bins; this still distinguishes ordinary
/// multi-second stalls from rare extreme pauses with fixed memory.
const OVERFLOW_BUCKET_COUNT: usize = 64;

/// A sample is "late" when its observed interval exceeds the requested interval by
/// this factor. Loose enough not to flag ordinary scheduler jitter.
const LATE_FACTOR_NUMERATOR: u64 = 3;
const LATE_FACTOR_DENOMINATOR: u64 = 2;

/// Percentiles of the observed inter-sample interval, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct IntervalPercentiles {
    pub p50: f64,
    pub p95: f64,
    pub max: f64,
}

/// Serializable timing summary embedded in the session manifest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimingSummary {
    /// Timing values describe only the latest collector process after a restart.
    #[serde(default = "timing_scope_default")]
    pub scope: String,
    pub poll_interval_ms_requested: u64,
    pub sample_count: u64,
    pub observed_interval_ms: IntervalPercentiles,
    pub late_sample_count: u64,
    pub skipped_tick_estimate: u64,
    /// Clock used for the interval measurements above.
    pub elapsed_basis: String,
    /// Clock used for the `timestamp_ms` column in the Parquet batches.
    pub row_timestamp_basis: String,
}

fn timing_scope_default() -> String {
    "latest_process".to_owned()
}

/// Bounded-memory tracker for inter-sample intervals.
///
/// Memory is constant regardless of run length: a multi-hour 5 ms capture produces
/// millions of samples but still only touches the fixed bucket array.
pub struct TimingStats {
    requested_ms: u64,
    sample_count: u64,
    buckets: Vec<u64>,
    tail_buckets: Vec<u64>,
    overflow_buckets: Vec<u64>,
    max_us: u64,
    late: u64,
    skipped: u64,
    last: Option<Instant>,
    last_scheduled: Option<Instant>,
}

impl TimingStats {
    pub fn new(requested_ms: u64) -> Self {
        // `parse_poll_interval_ms` rejects zero, so this only fires on a direct
        // construction. Zero would make every `skipped_tick_estimate` division
        // return `None`, silently reporting zero skipped ticks forever rather
        // than surfacing the misconfiguration.
        debug_assert!(requested_ms > 0, "TimingStats needs a non-zero cadence");
        Self {
            requested_ms,
            sample_count: 0,
            buckets: vec![0; BUCKET_COUNT],
            tail_buckets: vec![0; TAIL_BUCKET_COUNT],
            overflow_buckets: vec![0; OVERFLOW_BUCKET_COUNT],
            max_us: 0,
            late: 0,
            skipped: 0,
            last: None,
            last_scheduled: None,
        }
    }

    /// Record one poll tick. The first call establishes the baseline and
    /// contributes no interval.
    pub fn record(&mut self, now: Instant) {
        self.sample_count += 1;
        if let Some(previous) = self.last {
            let delta_us = now
                .duration_since(previous)
                .as_micros()
                .try_into()
                .unwrap_or(u64::MAX);
            self.record_interval_us(delta_us);
        }
        self.last = Some(now);
    }

    /// Interval ingestion split out from the clock so it can be unit tested.
    fn record_interval_us(&mut self, delta_us: u64) {
        // Checked, not `as`: on a 32-bit target an extreme stall would otherwise
        // wrap into a small index and be misfiled as a fast sample — corrupting
        // exactly the tail the histogram exists to measure. Every index below is
        // then used through `get_mut`, so the `usize::MAX` saturation lands in the
        // overflow tier rather than panicking.
        let index = usize::try_from(delta_us / BUCKET_US).unwrap_or(usize::MAX);
        let tail_index = delta_us
            .checked_sub(BUCKET_COUNT as u64 * BUCKET_US)
            .map(|above_fast| above_fast / TAIL_BUCKET_US)
            .and_then(|i| usize::try_from(i).ok());

        if let Some(bucket) = self.buckets.get_mut(index) {
            *bucket += 1;
        } else if let Some(bucket) = tail_index
            .filter(|_| delta_us < TAIL_CEILING_US)
            .and_then(|i| self.tail_buckets.get_mut(i))
        {
            *bucket += 1;
        } else {
            let mut upper = TAIL_CEILING_US;
            for (index, bucket) in self.overflow_buckets.iter_mut().enumerate() {
                upper = upper.saturating_mul(2);
                if delta_us < upper || index + 1 == OVERFLOW_BUCKET_COUNT {
                    *bucket += 1;
                    break;
                }
            }
        }
        self.max_us = self.max_us.max(delta_us);

        let requested_us = self.requested_ms.saturating_mul(1000);
        if requested_us > 0 {
            let late_threshold =
                requested_us.saturating_mul(LATE_FACTOR_NUMERATOR) / LATE_FACTOR_DENOMINATOR;
            if delta_us > late_threshold {
                self.late += 1;
            }
        }
    }

    /// Record the deadline Tokio delivered, separately from observed sample time.
    pub fn record_scheduled_tick(&mut self, now: Instant) {
        if let Some(previous) = self.last_scheduled {
            let delta_us: u64 = now
                .duration_since(previous)
                .as_micros()
                .try_into()
                .unwrap_or(u64::MAX);
            let requested_us = self.requested_ms.saturating_mul(1000);
            if let Some(elapsed_ticks) = delta_us.checked_div(requested_us) {
                self.skipped = self.skipped.saturating_add(elapsed_ticks.saturating_sub(1));
            }
        }
        self.last_scheduled = Some(now);
    }

    /// Total intervals recorded (one fewer than the sample count).
    fn interval_count(&self) -> u64 {
        self.buckets.iter().copied().sum::<u64>()
            + self.tail_buckets.iter().copied().sum::<u64>()
            + self.overflow_buckets.iter().copied().sum::<u64>()
    }

    /// Interpolate a percentile out of the histogram, in milliseconds.
    ///
    /// Returns the upper edge of the bucket where the cumulative count crosses the
    /// target, so the reported value is never an underestimate of the real one.
    fn percentile_ms(&self, fraction: f64) -> f64 {
        let total = self.interval_count();
        if total == 0 {
            return 0.0;
        }
        // `ceil` so p50 of a single interval reports that interval, not zero.
        let target = ((total as f64) * fraction).ceil().max(1.0) as u64;
        let mut cumulative = 0u64;
        for (index, count) in self.buckets.iter().enumerate() {
            cumulative += *count;
            if cumulative >= target {
                return ((index as u64 + 1) * BUCKET_US) as f64 / 1000.0;
            }
        }
        for (index, count) in self.tail_buckets.iter().enumerate() {
            cumulative += *count;
            if cumulative >= target {
                let upper = (BUCKET_COUNT as u64 * BUCKET_US) + (index as u64 + 1) * TAIL_BUCKET_US;
                return upper as f64 / 1000.0;
            }
        }
        let mut upper = TAIL_CEILING_US;
        for count in &self.overflow_buckets {
            cumulative += *count;
            upper = upper.saturating_mul(2);
            if cumulative >= target {
                return upper as f64 / 1000.0;
            }
        }
        // Defensive fallback for a value beyond the final saturated bucket.
        self.max_us as f64 / 1000.0
    }

    pub fn summary(&self) -> TimingSummary {
        TimingSummary {
            scope: timing_scope_default(),
            poll_interval_ms_requested: self.requested_ms,
            sample_count: self.sample_count,
            observed_interval_ms: IntervalPercentiles {
                p50: self.percentile_ms(0.50),
                p95: self.percentile_ms(0.95),
                max: self.max_us as f64 / 1000.0,
            },
            late_sample_count: self.late,
            skipped_tick_estimate: self.skipped,
            elapsed_basis: "monotonic".to_owned(),
            row_timestamp_basis: "wall_clock_utc".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats_from_intervals(requested_ms: u64, intervals_us: &[u64]) -> TimingStats {
        let mut stats = TimingStats::new(requested_ms);
        // Mirror `record`'s bookkeeping without needing a real clock.
        stats.sample_count = intervals_us.len() as u64 + 1;
        for interval in intervals_us {
            stats.record_interval_us(*interval);
        }
        stats
    }

    #[test]
    fn empty_stats_are_all_zero() {
        let summary = TimingStats::new(5).summary();
        assert_eq!(summary.sample_count, 0);
        assert_eq!(summary.observed_interval_ms.p50, 0.0);
        assert_eq!(summary.observed_interval_ms.max, 0.0);
        assert_eq!(summary.late_sample_count, 0);
        assert_eq!(summary.skipped_tick_estimate, 0);
    }

    #[test]
    fn steady_five_ms_stream_reports_five_ms() {
        let stats = stats_from_intervals(5, &[5_000; 100]);
        let summary = stats.summary();
        assert_eq!(summary.sample_count, 101);
        assert_eq!(summary.observed_interval_ms.p50, 5.1);
        assert_eq!(summary.observed_interval_ms.p95, 5.1);
        assert_eq!(summary.observed_interval_ms.max, 5.0);
        assert_eq!(summary.late_sample_count, 0);
        assert_eq!(summary.skipped_tick_estimate, 0);
    }

    #[test]
    fn late_samples_counted_past_one_and_a_half_times_requested() {
        // 7 ms is under the 7.5 ms threshold; 8 ms is over it.
        let stats = stats_from_intervals(5, &[5_000, 7_000, 8_000]);
        assert_eq!(stats.summary().late_sample_count, 1);
    }

    #[test]
    fn skipped_ticks_estimated_from_interval_ratio() {
        // A 23 ms gap at a 5 ms cadence means 4 intervals' worth elapsed, so 3 ticks
        // were skipped.
        let mut stats = TimingStats::new(5);
        let base = Instant::now();
        stats.record_scheduled_tick(base);
        stats.record_scheduled_tick(base + std::time::Duration::from_millis(20));
        assert_eq!(stats.summary().skipped_tick_estimate, 3);
    }

    /// Every bucket tier must be reachable without panicking, including the
    /// saturating index the checked conversion can produce.
    #[test]
    fn extreme_intervals_land_in_overflow_without_panicking() {
        let mut stats = TimingStats::new(5);
        for delta in [
            0u64,
            1,
            BUCKET_US,
            TAIL_CEILING_US - 1,
            TAIL_CEILING_US,
            u64::MAX,
        ] {
            stats.record_interval_us(delta);
        }
        // Reaching here at all is the assertion: an out-of-range index would have
        // panicked. `max_us` confirms the extreme sample was still recorded.
        assert_eq!(stats.max_us, u64::MAX);
    }

    #[test]
    fn max_is_exact_even_when_it_overflows_the_histogram() {
        // 250 ms is past the 100 ms primary range, but max must still be reported
        // precisely rather than clamped to a histogram edge.
        let stats = stats_from_intervals(5, &[5_000, 250_000]);
        let summary = stats.summary();
        assert_eq!(summary.observed_interval_ms.max, 250.0);
        assert_eq!(stats.tail_buckets.iter().sum::<u64>(), 1);
    }

    #[test]
    fn p95_retains_tail_distribution_above_the_primary_histogram() {
        // The 5 s outlier must not replace the p95 of the nine 110 ms stalls.
        let mut intervals = vec![5_000u64; 90];
        intervals.extend(std::iter::repeat_n(110_000u64, 9));
        intervals.push(5_000_000);
        let summary = stats_from_intervals(5, &intervals).summary();
        assert_eq!(summary.observed_interval_ms.p95, 111.0);
        assert_eq!(summary.observed_interval_ms.max, 5_000.0);
    }

    #[test]
    fn p95_tracks_the_tail_not_the_median() {
        // 10% slow samples: the nearest-rank p95 lands in the slow bucket while the
        // median stays fast. (At exactly 5% slow it would *not* — rank 95 of 100 is
        // still the last fast sample. p95 by definition tolerates 5% of the tail.)
        let mut intervals = vec![5_000u64; 90];
        intervals.extend(std::iter::repeat_n(40_000u64, 10));
        let summary = stats_from_intervals(5, &intervals).summary();
        assert_eq!(summary.observed_interval_ms.p50, 5.1);
        assert_eq!(summary.observed_interval_ms.p95, 40.1);
        assert_eq!(summary.observed_interval_ms.max, 40.0);
    }

    #[test]
    fn p95_ignores_a_tail_smaller_than_five_percent() {
        // Exactly 5 slow samples in 100 must not move p95 — this is the boundary the
        // test above deliberately steps past, and getting it wrong would make the
        // manifest overstate jitter.
        let mut intervals = vec![5_000u64; 95];
        intervals.extend(std::iter::repeat_n(40_000u64, 5));
        let summary = stats_from_intervals(5, &intervals).summary();
        assert_eq!(summary.observed_interval_ms.p95, 5.1);
        assert_eq!(
            summary.observed_interval_ms.max, 40.0,
            "max must still surface the outliers p95 smooths over"
        );
    }

    #[test]
    fn record_uses_monotonic_clock_and_counts_samples() {
        let mut stats = TimingStats::new(5);
        let base = Instant::now();
        stats.record(base);
        // First tick establishes the baseline only.
        assert_eq!(stats.interval_count(), 0);
        stats.record(base + std::time::Duration::from_millis(5));
        stats.record(base + std::time::Duration::from_millis(10));
        assert_eq!(stats.summary().sample_count, 3);
        assert_eq!(stats.interval_count(), 2);
    }

    #[test]
    fn summary_declares_its_clock_bases() {
        let summary = TimingStats::new(5).summary();
        assert_eq!(summary.scope, "latest_process");
        assert_eq!(summary.elapsed_basis, "monotonic");
        assert_eq!(summary.row_timestamp_basis, "wall_clock_utc");
    }
}
