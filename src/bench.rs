use std::{
    fmt,
    time::{Duration, Instant},
};

/// `#[divan::bench]` loop context.
///
/// Functions called within the benchmark loop should be `#[inline(always)]` to
/// ensure instruction cache locality.
///
/// Instances of this type are publicly accessible to generated code, so care
/// should be taken when making fields fully public.
pub struct Context {
    /// Recorded samples.
    pub(crate) samples: Vec<Sample>,

    /// The number of iterations between recording samples.
    pub iter_per_sample: u32,
}

impl Context {
    #[inline(always)]
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            // TODO: Pick these numbers dynamically.
            samples: Vec::with_capacity(1_000),
            iter_per_sample: 1_000,
        }
    }

    /// Returns the number of samples that should be taken.
    #[inline(always)]
    pub fn target_sample_count(&self) -> usize {
        self.samples.capacity()
    }

    /// Begins info measurement at the start of a loop.
    #[inline(always)]
    pub fn start_sample(&self) -> Instant {
        // Prevent other operations from affecting timing measurements.
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);

        Instant::now()
    }

    /// Records measurement info at the end of a loop.
    #[inline(always)]
    pub fn end_sample(&mut self, start: Instant) {
        let end = Instant::now();

        // Prevent other operations from affecting timing measurements.
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);

        self.samples.push(Sample { start, end });
    }

    pub fn compute_stats(&self) -> Option<Stats> {
        let sample_count = self.samples.len();
        let total_count = sample_count * self.iter_per_sample as usize;

        let first = self.samples.first()?;
        let last = self.samples.last()?;

        let total_duration = last.end.duration_since(first.start);
        let avg_duration = SmallDuration::average(total_duration, total_count as u128);

        let mut all_durations: Vec<SmallDuration> = self
            .samples
            .iter()
            .map(|sample| {
                SmallDuration::average(
                    sample.end.duration_since(sample.start),
                    self.iter_per_sample as u128,
                )
            })
            .collect();

        all_durations.sort_unstable();

        let min_duration = *all_durations.first().unwrap();
        let max_duration = *all_durations.last().unwrap();

        let median_duration = if sample_count % 2 == 0 {
            // Take average of two middle numbers.
            let a = all_durations[sample_count / 2];
            let b = all_durations[(sample_count / 2) - 1];

            SmallDuration { picos: (a.picos + b.picos) / 2 }
        } else {
            // Single middle number.
            all_durations[sample_count / 2]
        };

        Some(Stats {
            sample_count,
            total_count,
            total_duration,
            avg_duration,
            min_duration,
            max_duration,
            median_duration,
        })
    }
}

/// Measurement datum.
pub struct Sample {
    /// When the sample began.
    pub start: Instant,

    /// When the sample stopped.
    pub end: Instant,
}

/// Statistics from samples.
#[derive(Debug)]
pub struct Stats {
    /// Total number of samples taken.
    pub sample_count: usize,

    /// Total number of iterations (`sample_count * iter_per_sample`).
    pub total_count: usize,

    /// The total amount of time spent benchmarking.
    pub total_duration: Duration,

    /// Mean time taken by all iterations.
    pub avg_duration: SmallDuration,

    /// The minimum amount of time taken by an iteration.
    pub min_duration: SmallDuration,

    /// The maximum amount of time taken by an iteration.
    pub max_duration: SmallDuration,

    /// Midpoint time taken by an iteration.
    pub median_duration: SmallDuration,
}

/// [Picosecond](https://en.wikipedia.org/wiki/Picosecond)-precise [`Duration`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SmallDuration {
    picos: u128,
}

impl fmt::Debug for SmallDuration {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // `Duration` has no notion of picoseconds, so we manually format
        // picoseconds and nanoseconds ourselves.
        if self.picos < 1_000 {
            write!(f, "{}ps", self.picos)
        } else if self.picos < 1_000_000 {
            let nanos = self.picos as f64 / 1_000.0;
            write!(f, "{}ns", nanos)
        } else {
            Duration::from_nanos((self.picos / 1_000) as u64).fmt(f)
        }
    }
}

impl SmallDuration {
    /// Computes the average of a duration over a number of elements.
    fn average(duration: Duration, n: u128) -> Self {
        Self { picos: (duration.as_nanos() * 1_000) / n }
    }
}

/// Defers `Drop` of items produced while benchmarking.
pub struct DropStore<T> {
    items: Vec<T>,
}

#[allow(missing_docs)]
impl<T> DropStore<T> {
    const IS_NO_OP: bool = !std::mem::needs_drop::<T>();

    #[inline]
    pub fn with_capacity(capacity: usize) -> Self {
        Self { items: if Self::IS_NO_OP { Vec::new() } else { Vec::with_capacity(capacity) } }
    }

    /// Prepares the store for storing a sample.
    #[inline(always)]
    pub fn prepare(&mut self, capacity: usize) {
        if !Self::IS_NO_OP {
            self.items.clear();
            self.items.reserve_exact(capacity);
        }
    }

    #[inline(always)]
    pub fn push(&mut self, item: T) {
        if !Self::IS_NO_OP {
            self.items.push(item);
        }
    }
}