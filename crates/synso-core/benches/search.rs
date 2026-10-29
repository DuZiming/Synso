//! Task 20 benchmark: the ten query classes of the search budget, timed by
//! criterion over the 1M-entry synthetic hot index
//! ([`synso_core::HotIndex::synthetic`]). The hard p95 < 50ms asserts live
//! in `tests/perf.rs` (hand-rolled timing over the same queries); this
//! bench exists for distribution-grade numbers — mean, slope, outlier
//! counts — on exactly the same probes, so a regression seen in the tests
//! can be characterized here without re-plumbing.
//!
//! Run with: `cargo bench -p synso-core` (release profile by definition).

use std::hint::black_box;
use std::sync::OnceLock;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion};

use synso_core::{search_index, HotIndex};

/// The budgeted index size (the spec's 1M-file target).
const INDEX_ENTRIES: u64 = 1_000_000;

/// Built once per process (~1-2s) and shared by every benchmark below.
fn index() -> &'static HotIndex {
    static INDEX: OnceLock<HotIndex> = OnceLock::new();
    INDEX.get_or_init(|| HotIndex::synthetic(INDEX_ENTRIES))
}

/// The ten query classes the 50ms p95 budget is asserted against in
/// tests/perf.rs. One probe per class; `path:projects` and `ext:pdf
/// size:>1mb` exercise the modifier fast paths, `""` the recency ranking
/// and `"e"` the high-hit-count worst case (~most of the corpus matches).
const QUERIES: &[(&str, &str)] = &[
    ("short_prefix", "rep"),
    ("cjk_substring", "报告"),
    ("pinyin_initials", "bg"),
    ("pinyin_prefix", "baog"),
    ("plain_substring", "report"),
    ("long_query", "annual-report-2026-final-edition"),
    ("modifier_combo", "ext:pdf size:>1mb"),
    ("path_filter", "path:projects"),
    ("empty_recent", ""),
    ("common_substring", "e"),
];

fn bench_search(c: &mut Criterion) {
    let hot = index();
    let mut group = c.benchmark_group("1m_search");
    group.warm_up_time(Duration::from_millis(600));
    group.measurement_time(Duration::from_secs(4));
    for (name, query) in QUERIES {
        group.bench_function(*name, |b| {
            b.iter(|| search_index(black_box(hot), black_box(query), 0, 50))
        });
    }
    group.finish();
}

criterion_group!(benches, bench_search);
criterion_main!(benches);
