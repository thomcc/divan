use std::ptr::NonNull;

use crate::{bench::BenchContext, Bencher};

mod generic;
mod list;
mod meta;
mod tree;

pub use self::{
    generic::{EntryConst, EntryType, GenericBenchEntry},
    list::EntryList,
    meta::{EntryLocation, EntryMeta},
};
pub(crate) use tree::EntryTree;

/// Benchmark entries generated by `#[divan::bench]`.
///
/// Note: generic-type benchmark entries are instead stored in `GROUP_ENTRIES`
/// in `generic_benches`.
#[cfg(any(windows, target_os = "linux", target_os = "android"))]
pub static BENCH_ENTRIES: EntryList<BenchEntry> = EntryList::root();

/// Benchmark entries generated by `#[divan::bench]`.
///
/// Note: generic-type benchmark entries are instead stored in `GROUP_ENTRIES`
/// in `generic_benches`.
#[cfg(not(any(windows, target_os = "linux", target_os = "android")))]
#[cfg_attr(
    not(any(windows, target_os = "linux", target_os = "android")),
    linkme::distributed_slice
)]
pub static BENCH_ENTRIES: [BenchEntry] = [..];

/// Group entries generated by `#[divan::bench_group]`.
#[cfg(any(windows, target_os = "linux", target_os = "android"))]
pub static GROUP_ENTRIES: EntryList<GroupEntry> = EntryList::root();

/// Group entries generated by `#[divan::bench_group]`.
#[cfg(not(any(windows, target_os = "linux", target_os = "android")))]
#[cfg_attr(
    not(any(windows, target_os = "linux", target_os = "android")),
    linkme::distributed_slice
)]
pub static GROUP_ENTRIES: [GroupEntry] = [..];

/// Compile-time entry for a benchmark, generated by `#[divan::bench]`.
pub struct BenchEntry {
    /// Entry metadata.
    pub meta: EntryMeta,

    /// The benchmarking function.
    pub bench: fn(Bencher),
}

/// Compile-time entry for a benchmark group, generated by
/// `#[divan::bench_group]` or a generic-type `#[divan::bench]`.
pub struct GroupEntry {
    /// Entry metadata.
    pub meta: EntryMeta,

    /// Generic `#[divan::bench]` entries.
    ///
    /// This is two-dimensional to make code generation simpler. The outer
    /// dimension corresponds to types and the inner dimension corresponds to
    /// constants.
    pub generic_benches: Option<&'static [&'static [GenericBenchEntry]]>,
}

impl GroupEntry {
    pub(crate) fn generic_benches_iter(&self) -> impl Iterator<Item = &'static GenericBenchEntry> {
        self.generic_benches.unwrap_or_default().iter().flat_map(|benches| benches.iter())
    }
}

/// `BenchEntry` or `GenericBenchEntry`.
#[derive(Clone, Copy)]
pub(crate) enum AnyBenchEntry<'a> {
    Bench(&'a BenchEntry),
    GenericBench(&'a GenericBenchEntry),
}

impl<'a> AnyBenchEntry<'a> {
    /// Returns a pointer to use as the identity of the entry.
    #[inline]
    pub fn entry_addr(self) -> NonNull<()> {
        match self {
            Self::Bench(entry) => NonNull::from(entry).cast(),
            Self::GenericBench(entry) => NonNull::from(entry).cast(),
        }
    }

    /// Runs the benchmarks in this entry.
    ///
    /// For each benchmark, `with_context` is called once.
    #[inline]
    pub fn bench(self, with_context: &mut dyn FnMut(&mut dyn FnMut(&mut BenchContext))) {
        match self {
            Self::Bench(BenchEntry { bench, .. })
            | Self::GenericBench(GenericBenchEntry { bench, .. }) => {
                with_context(&mut |context| bench(Bencher::new(context)));
            }
        }
    }

    #[inline]
    pub fn meta(self) -> &'a EntryMeta {
        match self {
            Self::Bench(entry) => &entry.meta,
            Self::GenericBench(entry) => &entry.group.meta,
        }
    }

    #[inline]
    pub fn raw_name(self) -> &'a str {
        match self {
            Self::Bench(entry) => entry.meta.raw_name,
            Self::GenericBench(entry) => entry.raw_name(),
        }
    }

    #[inline]
    pub fn display_name(self) -> &'a str {
        match self {
            Self::Bench(entry) => entry.meta.display_name,
            Self::GenericBench(entry) => entry.display_name(),
        }
    }
}
