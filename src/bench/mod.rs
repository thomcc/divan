use std::{
    cell::UnsafeCell,
    fmt,
    mem::{self, MaybeUninit},
};

use crate::{
    black_box,
    counter::{AnyCounter, CounterCollection, IntoCounter, KnownCounterKind, MaxCountUInt},
    divan::SharedContext,
    stats::{Sample, SampleCollection, Stats},
    time::{FineDuration, Timestamp, UntaggedTimestamp},
    util,
};

// Used for intra-doc links.
#[allow(unused)]
use crate::counter::Bytes;

#[cfg(test)]
mod tests;

mod defer;
mod options;

use defer::{DeferSlot, DeferStore};
pub use options::BenchOptions;

pub(crate) const DEFAULT_SAMPLE_COUNT: u32 = 100;

/// Enables contextual benchmarking in [`#[divan::bench]`](attr.bench.html).
///
/// # Examples
///
/// ```
/// use divan::{Bencher, black_box};
///
/// #[divan::bench]
/// fn copy_from_slice(bencher: Bencher) {
///     // Input and output buffers get used in the closure.
///     let src = (0..100).collect::<Vec<i32>>();
///     let mut dst = vec![0; src.len()];
///
///     bencher.bench(|| {
///         black_box(&mut dst).copy_from_slice(black_box(&src));
///     });
/// }
/// ```
#[must_use = "a benchmark function must be registered"]
pub struct Bencher<'a, 'b, C = BencherConfig> {
    pub(crate) context: &'a mut BenchContext<'b>,
    pub(crate) config: C,
}

/// Public-in-private type for statically-typed `Bencher` configuration.
///
/// This enables configuring `Bencher` using the builder pattern with zero
/// runtime cost.
pub struct BencherConfig<GenI = ()> {
    gen_input: GenI,
}

impl<C> fmt::Debug for Bencher<'_, '_, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bencher").finish_non_exhaustive()
    }
}

impl<'a, 'b> Bencher<'a, 'b> {
    #[inline]
    pub(crate) fn new(context: &'a mut BenchContext<'b>) -> Self {
        Self { context, config: BencherConfig { gen_input: () } }
    }
}

impl<'a, 'b> Bencher<'a, 'b> {
    /// Benchmarks a function.
    ///
    /// # Examples
    ///
    /// ```
    /// #[divan::bench]
    /// fn bench(bencher: divan::Bencher) {
    ///     bencher.bench(|| {
    ///         // Benchmarked code...
    ///     });
    /// }
    /// ```
    pub fn bench<O, B>(self, mut benched: B)
    where
        B: FnMut() -> O,
    {
        // Reusing `bench_values` for a zero-sized non-drop input type should
        // have no overhead.
        self.with_inputs(|| ()).bench_values(|_: ()| benched());
    }

    /// Generate inputs for the [benchmarked function](#input-bench).
    ///
    /// Time spent generating inputs does not affect benchmark timing.
    ///
    /// # Examples
    ///
    /// ```
    /// #[divan::bench]
    /// fn bench(bencher: divan::Bencher) {
    ///     bencher
    ///         .with_inputs(|| {
    ///             // Generate input:
    ///             String::from("...")
    ///         })
    ///         .bench_values(|s| {
    ///             // Use input by-value:
    ///             s + "123"
    ///         });
    /// }
    /// ```
    pub fn with_inputs<I, G>(self, gen_input: G) -> Bencher<'a, 'b, BencherConfig<G>>
    where
        G: FnMut() -> I,
    {
        Bencher { context: self.context, config: BencherConfig { gen_input } }
    }
}

impl<'a, 'b, GenI> Bencher<'a, 'b, BencherConfig<GenI>> {
    /// Assign a [`Counter`](crate::counter::Counter) for all iterations of the
    /// benchmarked function.
    ///
    /// This will either:
    /// - Assign a new counter
    /// - Override an existing counter of the same type
    ///
    /// If the counter depends on [generated inputs](Self::with_inputs), use
    /// [`Bencher::input_counter`] instead.
    ///
    /// If context is not needed, the counter can instead be set via
    /// [`#[divan::bench(counters = ...)]`](macro@crate::bench#counters).
    ///
    /// # Examples
    ///
    /// ```
    /// use divan::{Bencher, counter::Bytes};
    ///
    /// #[divan::bench]
    /// fn char_count(bencher: Bencher) {
    ///     let s: String = // ...
    ///     # String::new();
    ///
    ///     bencher
    ///         .counter(Bytes::of_str(&s))
    ///         .bench(|| {
    ///             divan::black_box(&s).chars().count()
    ///         });
    /// }
    /// ```
    #[doc(alias = "throughput")]
    pub fn counter<C>(self, counter: C) -> Self
    where
        C: IntoCounter,
    {
        let counter = AnyCounter::new(counter);
        self.context.counters.set_counter(counter);
        self
    }
}

/// <span id="input-bench"></span> Benchmark over [generated inputs](Self::with_inputs).
impl<'a, 'b, I, GenI> Bencher<'a, 'b, BencherConfig<GenI>>
where
    GenI: FnMut() -> I,
{
    /// Create a [`Counter`](crate::counter::Counter) for each input of the
    /// benchmarked function.
    ///
    /// This will either:
    /// - Assign a new counter
    /// - Override an existing counter of the same type
    ///
    /// If the counter is constant, use [`Bencher::counter`] instead.
    ///
    /// # Examples
    ///
    /// The following example emits info for the number of bytes processed when
    /// benchmarking [`char`-counting](std::str::Chars::count). The byte count
    /// is gotten by calling [`Bytes::of_str`] on each iteration's input
    /// [`String`].
    ///
    /// ```
    /// use divan::{Bencher, counter::Bytes};
    ///
    /// #[divan::bench]
    /// fn char_count(bencher: Bencher) {
    ///     bencher
    ///         .with_inputs(|| -> String {
    ///             // ...
    ///             # String::new()
    ///         })
    ///         .input_counter(Bytes::of_str)
    ///         .bench_refs(|s| {
    ///             s.chars().count()
    ///         });
    /// }
    /// ```
    pub fn input_counter<C, F>(self, make_counter: F) -> Self
    where
        F: FnMut(&I) -> C + 'static,
        C: IntoCounter,
    {
        self.context.counters.set_input_counter(make_counter);
        self
    }

    /// Benchmarks a function over per-iteration [generated inputs](Self::with_inputs),
    /// provided by-value.
    ///
    /// Per-iteration means the benchmarked function is called exactly once for
    /// each generated input.
    ///
    /// # Examples
    ///
    /// ```
    /// #[divan::bench]
    /// fn bench(bencher: divan::Bencher) {
    ///     bencher
    ///         .with_inputs(|| {
    ///             // Generate input:
    ///             String::from("...")
    ///         })
    ///         .bench_values(|s| {
    ///             // Use input by-value:
    ///             s + "123"
    ///         });
    /// }
    /// ```
    pub fn bench_values<O, B>(self, mut benched: B)
    where
        B: FnMut(I) -> O,
    {
        self.context.bench_loop(
            self.config,
            |input| {
                // SAFETY: Input is guaranteed to be initialized and not
                // currently referenced by anything else.
                let input = unsafe { input.get().read().assume_init() };

                benched(input)
            },
            // Input ownership is transferred to `benched`.
            |_input| {},
        );
    }

    /// Benchmarks a function over per-iteration [generated inputs](Self::with_inputs),
    /// provided by-reference.
    ///
    /// Per-iteration means the benchmarked function is called exactly once for
    /// each generated input.
    ///
    /// # Examples
    ///
    /// ```
    /// #[divan::bench]
    /// fn bench(bencher: divan::Bencher) {
    ///     bencher
    ///         .with_inputs(|| {
    ///             // Generate input:
    ///             String::from("...")
    ///         })
    ///         .bench_refs(|s| {
    ///             // Use input by-reference:
    ///             *s += "123";
    ///         });
    /// }
    /// ```
    pub fn bench_refs<O, B>(self, mut benched: B)
    where
        B: FnMut(&mut I) -> O,
    {
        // TODO: Allow `O` to reference `&mut I` as long as `I` outlives `O`.
        self.context.bench_loop(
            self.config,
            |input| {
                // SAFETY: Input is guaranteed to be initialized and not
                // currently referenced by anything else.
                let input = unsafe { (*input.get()).assume_init_mut() };

                benched(input)
            },
            // Input ownership was not transferred to `benched`.
            |input| {
                // SAFETY: This function is called after `benched` outputs are
                // dropped, so we have exclusive access.
                unsafe { (*input.get()).assume_init_drop() }
            },
        );
    }
}

/// State machine for how the benchmark is being run.
#[derive(Clone, Copy)]
pub(crate) enum BenchMode {
    /// The benchmark is being run as `--test`.
    ///
    /// Don't collect samples and run exactly once.
    Test,

    /// Scale `sample_size` to determine the right size for collecting.
    Tune { sample_size: u32 },

    /// Simply collect samples.
    Collect { sample_size: u32 },
}

impl BenchMode {
    #[inline]
    pub fn is_test(self) -> bool {
        matches!(self, Self::Test)
    }

    #[inline]
    pub fn is_tune(self) -> bool {
        matches!(self, Self::Tune { .. })
    }

    #[inline]
    pub fn is_collect(self) -> bool {
        matches!(self, Self::Collect { .. })
    }

    #[inline]
    pub fn sample_size(self) -> u32 {
        match self {
            Self::Test => 1,
            Self::Tune { sample_size, .. } | Self::Collect { sample_size, .. } => sample_size,
        }
    }
}

/// `#[divan::bench]` loop context.
///
/// Functions called within the benchmark loop should be `#[inline(always)]` to
/// ensure instruction cache locality.
pub(crate) struct BenchContext<'a> {
    shared_context: &'a SharedContext,

    /// User-configured options.
    pub options: &'a BenchOptions,

    /// Whether the benchmark loop was started.
    pub did_run: bool,

    /// Recorded samples.
    samples: SampleCollection,

    /// Per-iteration counters grouped by sample.
    counters: CounterCollection,
}

impl<'a> BenchContext<'a> {
    /// Creates a new benchmarking context.
    pub fn new(shared_context: &'a SharedContext, options: &'a BenchOptions) -> Self {
        Self {
            shared_context,
            options,
            did_run: false,
            samples: SampleCollection::default(),
            counters: options.counters.to_collection(),
        }
    }

    /// Runs the loop for benchmarking `benched`.
    ///
    /// # Safety
    ///
    /// When `benched` is called:
    /// - `I` is guaranteed to be initialized.
    /// - No external `&I` or `&mut I` exists.
    ///
    /// When `drop_input` is called:
    /// - All instances of `O` returned from `benched` have been dropped.
    /// - The same guarantees for `I` apply as in `benched`, unless `benched`
    ///   escaped references to `I`.
    pub fn bench_loop<I, O>(
        &mut self,
        config: BencherConfig<impl FnMut() -> I>,
        benched: impl FnMut(&UnsafeCell<MaybeUninit<I>>) -> O,
        drop_input: impl Fn(&UnsafeCell<MaybeUninit<I>>),
    ) {
        self.did_run = true;

        let mut current_mode = self.initial_mode();
        let is_test = current_mode.is_test();

        // The time spent benchmarking, in picoseconds.
        //
        // Unless `skip_ext_time` is set, this includes time external to
        // `benched`, such as time spent generating inputs and running drop.
        let mut elapsed_picos: u128 = 0;

        // The minimum time for benchmarking, in picoseconds.
        let min_picos = self.options.min_time().picos;

        // The remaining time left for benchmarking, in picoseconds.
        let max_picos = self.options.max_time().picos;

        // Don't bother running if user specifies 0 max time or 0 samples.
        if max_picos == 0 || !self.options.has_samples() {
            return;
        }

        let timer = self.shared_context.timer;
        let timer_kind = timer.kind();

        let mut record_sample = self.sample_recorder(config.gen_input, benched, drop_input);

        let mut rem_samples = if current_mode.is_collect() {
            Some(self.options.sample_count.unwrap_or(DEFAULT_SAMPLE_COUNT))
        } else {
            None
        };

        // Only measure precision if we need to tune sample size.
        let timer_precision =
            if current_mode.is_tune() { timer.precision() } else { FineDuration::default() };

        if !is_test {
            self.samples.all.reserve(self.options.sample_count.unwrap_or(1) as usize);
        }

        let skip_ext_time = self.options.skip_ext_time.unwrap_or_default();
        let initial_start = if skip_ext_time { None } else { Some(Timestamp::start(timer_kind)) };

        while {
            // Conditions for when sampling is over:
            if elapsed_picos >= max_picos {
                // Depleted the benchmarking time budget. This is a strict
                // condition regardless of sample count and minimum time.
                false
            } else if rem_samples.unwrap_or(1) > 0 {
                // More samples expected.
                true
            } else {
                // Continue if we haven't reached the time floor.
                elapsed_picos < min_picos
            }
        } {
            let sample_size = current_mode.sample_size();
            self.samples.sample_size = sample_size;

            let mut sample_counter_totals: [u128; KnownCounterKind::COUNT] =
                [0; KnownCounterKind::COUNT];

            // Updates per-input counter info for this sample.
            let mut count_input = |input: &I| {
                for counter_kind in KnownCounterKind::ALL {
                    // SAFETY: The `I` type cannot change since `with_inputs`
                    // cannot be called more than once on the same `Bencher`.
                    if let Some(count) =
                        unsafe { self.counters.get_input_count(counter_kind, input) }
                    {
                        let total = &mut sample_counter_totals[counter_kind as usize];
                        *total = (*total).saturating_add(count as u128);
                    }
                }
            };

            let [sample_start, sample_end] = record_sample(sample_size as usize, &mut count_input);

            // If testing, exit the benchmarking loop immediately after timing a
            // single run.
            if is_test {
                break;
            }

            let mut raw_duration = sample_end.duration_since(sample_start, timer);

            // Round up to timer precision if the duration is zero.
            //
            // This is deliberately done again later after subtracting
            // `sample_overhead`.
            if raw_duration.is_zero() {
                raw_duration = timer_precision;
            }

            // TODO: Make tuning be less influenced by early runs. Currently if
            // early runs are very quick but later runs are slow, benchmarking
            // will take a very long time.
            //
            // TODO: Make `sample_size` consider time generating inputs and
            // dropping inputs/outputs. Currently benchmarks like
            // `Bencher::bench_refs(String::clear)` take a very long time.
            if current_mode.is_tune() {
                // Clear previous smaller samples.
                self.samples.all.clear();
                self.counters.clear_input_counts();

                // If within 100x timer precision, continue tuning.
                let precision_multiple = raw_duration.picos / timer_precision.picos;
                if precision_multiple <= 100 {
                    current_mode = BenchMode::Tune { sample_size: sample_size * 2 };
                } else {
                    current_mode = BenchMode::Collect { sample_size };
                    rem_samples = Some(self.options.sample_count.unwrap_or(DEFAULT_SAMPLE_COUNT));
                }
            }

            // Account for the per-sample benchmarking overhead.
            let mut adjusted_duration = {
                let sample_overhead =
                    self.shared_context.bench_overhead.picos.saturating_mul(sample_size as u128);

                FineDuration { picos: raw_duration.picos.saturating_sub(sample_overhead) }
            };

            // Round up to timer precision if the duration is zero. We do this a
            // second time in case subtracting `sample_overhead` caused the
            // duration to become zero.
            if adjusted_duration.is_zero() {
                adjusted_duration = timer_precision;
            }

            self.samples.all.push(Sample { duration: adjusted_duration });

            // Insert per-input counter information.
            for counter_kind in KnownCounterKind::ALL {
                if !self.counters.uses_input_counts(counter_kind) {
                    continue;
                }

                let total_count = sample_counter_totals[counter_kind as usize];

                // This will not overflow `MaxCountUInt` because `total_count`
                // cannot exceed `MaxCountUInt::MAX * sample_size`.
                let per_iter_count = (total_count / sample_size as u128) as MaxCountUInt;

                self.counters.push_counter(AnyCounter::known(counter_kind, per_iter_count));
            }

            if let Some(rem_samples) = &mut rem_samples {
                *rem_samples = rem_samples.saturating_sub(1);
            }

            if let Some(initial_start) = initial_start {
                elapsed_picos = sample_end.duration_since(initial_start, timer).picos;
            } else {
                // Progress by at least 1ns to prevent extremely fast
                // functions from taking forever when `min_time` is set.
                let progress_picos = raw_duration.picos.max(1_000);
                elapsed_picos = elapsed_picos.saturating_add(progress_picos);
            }
        }
    }

    /// Returns a closure that takes the sample size and input counter, and then
    /// returns a newly recorded sample.
    fn sample_recorder<I, O>(
        &self,
        mut gen_input: impl FnMut() -> I,
        mut benched: impl FnMut(&UnsafeCell<MaybeUninit<I>>) -> O,
        drop_input: impl Fn(&UnsafeCell<MaybeUninit<I>>),
    ) -> impl FnMut(usize, &mut dyn FnMut(&I)) -> [Timestamp; 2] {
        // Defer:
        // - Usage of `gen_input` values.
        // - Drop destructor for `O`, preventing it from affecting sample
        //   measurements. Outputs are stored into a pre-allocated buffer during
        //   the sample loop. The allocation is reused between samples to reduce
        //   time spent between samples.
        let mut defer_store: DeferStore<I, O> = DeferStore::default();

        let timer_kind = self.shared_context.timer.kind();

        move |sample_size: usize, count_input: &mut dyn FnMut(&I)| {
            // The following logic chooses how to efficiently sample the
            // benchmark function once and assigns `sample_start`/`sample_end`
            // before/after the sample loop.
            //
            // NOTE: Testing and benchmarking should behave exactly the same
            // when getting the sample time span. We don't want to introduce
            // extra work that may worsen measurement quality for real
            // benchmarking.
            let sample_start: UntaggedTimestamp;
            let sample_end: UntaggedTimestamp;

            if (mem::size_of::<I>() == 0 && mem::size_of::<O>() == 0)
                || (mem::size_of::<I>() == 0 && !mem::needs_drop::<O>())
            {
                // Use a range instead of `defer_store` to make the benchmarking
                // loop cheaper.

                // Run `gen_input` the expected number of times in case it
                // updates external state used by `benched`.
                for _ in 0..sample_size {
                    let input = gen_input();
                    count_input(&input);

                    // Inputs are consumed/dropped later.
                    mem::forget(input);
                }

                sample_start = UntaggedTimestamp::start(timer_kind);

                // Sample loop:
                for _ in 0..sample_size {
                    // SAFETY: Input is a ZST, so we can construct one out of
                    // thin air.
                    let input = unsafe { UnsafeCell::new(MaybeUninit::<I>::zeroed()) };

                    mem::forget(black_box(benched(&input)));
                }

                sample_end = UntaggedTimestamp::end(timer_kind);

                // Drop outputs and inputs.
                for _ in 0..sample_size {
                    // Output only needs drop if ZST.
                    if mem::size_of::<O>() == 0 {
                        // SAFETY: Output is a ZST, so we can construct one out
                        // of thin air.
                        unsafe { _ = mem::zeroed::<O>() }
                    }

                    if mem::needs_drop::<I>() {
                        // SAFETY: Input is a ZST, so we can construct one out
                        // of thin air and not worry about aliasing.
                        unsafe { drop_input(&UnsafeCell::new(MaybeUninit::<I>::zeroed())) }
                    }
                }
            } else {
                defer_store.prepare(sample_size);

                match defer_store.slots() {
                    // Output needs to be dropped. We defer drop in the sample
                    // loop by inserting it into `defer_store`.
                    Ok(defer_slots_slice) => {
                        // Initialize and store inputs.
                        for DeferSlot { input, .. } in defer_slots_slice {
                            // SAFETY: We have exclusive access to `input`.
                            let input = unsafe { &mut *input.get() };
                            let input = input.write(gen_input());
                            count_input(input);
                        }

                        // Create iterator before the sample timing section to
                        // reduce benchmarking overhead.
                        let defer_slots_iter = black_box(defer_slots_slice.iter());

                        sample_start = UntaggedTimestamp::start(timer_kind);

                        // Sample loop:
                        for defer_slot in defer_slots_iter {
                            // SAFETY: All inputs in `defer_store` were
                            // initialized and we have exclusive access to the
                            // output slot.
                            unsafe {
                                let output = benched(&defer_slot.input);
                                *defer_slot.output.get() = MaybeUninit::new(output);
                            }

                            // PERF: `black_box` the slot address because:
                            // - It prevents `input` mutation from being
                            //   optimized out.
                            // - `black_box` writes its input to the stack.
                            //   Using the slot address instead of the output
                            //   by-value reduces overhead when `O` is a larger
                            //   type like `String` since then it will write a
                            //   single word instead of three words.
                            _ = black_box(defer_slot);
                        }

                        sample_end = UntaggedTimestamp::end(timer_kind);

                        // Drop outputs and inputs.
                        for DeferSlot { input, output } in defer_slots_slice {
                            // SAFETY: All outputs were initialized in the
                            // sample loop and we have exclusive access.
                            unsafe { (*output.get()).assume_init_drop() }

                            if mem::needs_drop::<I>() {
                                // SAFETY: The output was dropped and thus we
                                // have exclusive access to inputs.
                                unsafe { drop_input(input) }
                            }
                        }
                    }

                    // Output does not need to be dropped.
                    Err(defer_inputs_slice) => {
                        // Initialize and store inputs.
                        for input in defer_inputs_slice {
                            // SAFETY: We have exclusive access to `input`.
                            let input = unsafe { &mut *input.get() };
                            let input = input.write(gen_input());
                            count_input(input);
                        }

                        // Create iterator before the sample timing section to
                        // reduce benchmarking overhead.
                        let defer_inputs_iter = black_box(defer_inputs_slice.iter());

                        sample_start = UntaggedTimestamp::start(timer_kind);

                        // Sample loop:
                        for input in defer_inputs_iter {
                            // SAFETY: All inputs in `defer_store` were
                            // initialized.
                            _ = black_box(unsafe { benched(input) });
                        }

                        sample_end = UntaggedTimestamp::end(timer_kind);

                        // Drop inputs.
                        if mem::needs_drop::<I>() {
                            for input in defer_inputs_slice {
                                // SAFETY: We have exclusive access to inputs.
                                unsafe { drop_input(input) }
                            }
                        }
                    }
                }
            }

            // SAFETY: These values are guaranteed to be the correct variant
            // because they were created from the same `timer_kind`.
            unsafe {
                [sample_start.into_timestamp(timer_kind), sample_end.into_timestamp(timer_kind)]
            }
        }
    }

    #[inline]
    fn initial_mode(&self) -> BenchMode {
        if self.shared_context.action.is_test() {
            BenchMode::Test
        } else if let Some(sample_size) = self.options.sample_size {
            BenchMode::Collect { sample_size }
        } else {
            BenchMode::Tune { sample_size: 1 }
        }
    }

    pub fn compute_stats(&self) -> Stats {
        use crate::stats::StatsSet;

        let samples = &self.samples.all;
        let sample_count = samples.len();
        let sample_size = self.samples.sample_size;

        let total_count = self.samples.iter_count();

        let total_duration = self.samples.total_duration();
        let mean_duration = FineDuration {
            picos: total_duration.picos.checked_div(total_count as u128).unwrap_or_default(),
        };

        // Samples sorted by duration.
        let sorted_samples = self.samples.sorted_samples();
        let median_samples = util::slice_middle(&sorted_samples);

        let index_of_sample = |sample: &Sample| -> usize {
            // Safe pointer `offset_from`.
            let start = self.samples.all.as_ptr() as usize;
            let sample = sample as *const Sample as usize;
            (sample - start) / mem::size_of::<Sample>()
        };

        let counter_count_for_sample =
            |sample: &Sample, counter_kind: KnownCounterKind| -> Option<MaxCountUInt> {
                let counts = self.counters.counts(counter_kind);

                let index = if self.counters.uses_input_counts(counter_kind) {
                    index_of_sample(sample)
                } else {
                    0
                };

                counts.get(index).copied()
            };

        let min_duration =
            sorted_samples.first().map(|s| s.duration / sample_size).unwrap_or_default();
        let max_duration =
            sorted_samples.last().map(|s| s.duration / sample_size).unwrap_or_default();

        let median_duration = if median_samples.is_empty() {
            FineDuration::default()
        } else {
            let sum: u128 = median_samples.iter().map(|s| s.duration.picos).sum();
            FineDuration { picos: sum / median_samples.len() as u128 } / sample_size
        };

        let counts = KnownCounterKind::ALL.map(|counter_kind| {
            let median: MaxCountUInt = {
                let mut sum: u128 = 0;

                for sample in median_samples {
                    let sample_count = counter_count_for_sample(sample, counter_kind)? as u128;

                    // Saturating add in case `MaxUIntCount > u64`.
                    sum = sum.saturating_add(sample_count);
                }

                (sum / median_samples.len() as u128) as MaxCountUInt
            };

            Some(StatsSet {
                fastest: sorted_samples
                    .first()
                    .and_then(|s| counter_count_for_sample(s, counter_kind))?,
                slowest: sorted_samples
                    .last()
                    .and_then(|s| counter_count_for_sample(s, counter_kind))?,
                median,
                mean: self.counters.mean_count(counter_kind),
            })
        });

        Stats {
            sample_count: sample_count as u32,
            iter_count: total_count,
            time: StatsSet {
                mean: mean_duration,
                fastest: min_duration,
                slowest: max_duration,
                median: median_duration,
            },
            counts,
        }
    }
}
