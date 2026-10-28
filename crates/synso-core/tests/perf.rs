//! Task 20 release-build validation of the three spec budgets
//! (docs/superpowers/plans/2026-10-27-synso-v1.md):
//!
//! 1. `perf_1m_search_under_50ms` — keystroke-to-first-results: p95 of ten
//!    typical query classes over a 1M-entry synthetic hot index must stay
//!    under 50ms per class.
//! 2. `perf_scan_throughput_100k` — the 100 万文件首扫 < 5 分钟 budget
//!    scaled to a generated corpus: ≥ 3300 files/s (= 1M in 300s).
//! 3. `perf_interned_bytes_1m_under_250mb` — engine-side memory: the same
//!    1M index must intern under 250MB by the `interned_bytes` metric
//!    (the number `Engine::stats` reports; a 50MB headroom under the
//!    spec's 300MB line).
//!
//! plus `perf_obs_churn_search_latency`, the no-assert observation Task 17
//! flagged: search latency while a watcher batch applies (reader-vs-writer
//! lock contention), reported as p50/p95 with and without churn.
//!
//! These tests are `#[ignore]`d — the normal suite stays fast and
//! deterministic. Run the whole harness IN RELEASE (debug numbers are
//! contention-tainted and meaningless):
//!
//! ```text
//! cargo test --release -p synso-core --test perf -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Methodology notes:
//! - The 1M index is built once per process via `HotIndex::synthetic` —
//!   deterministic, mixed CJK/latin names, pinyin keys from the production
//!   generator — and shared between the search and memory tests.
//! - Each query class is warmed up, then sampled up to 200 times (6s cap);
//!   percentiles are taken over per-iteration wall times.
//! - The 100k scan fixture lives once under the OS temp dir
//!   (`synso-perf-fixture-100k`, marker file next to it) and is reused
//!   across runs; delete both to force regeneration.
//! - The scan itself is timed engine-side, from `start_full_scan` to
//!   `ScanFinished` — purge, walk, store writes, hot-index reload and the
//!   collections reconcile included, exactly the work a real first scan
//!   does.

use std::fs;
use std::hint::black_box;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;

use synso_core::{search_index, Engine, EngineEvent, EngineOptions, HotIndex};

/// The spec's 1M-file target.
const INDEX_ENTRIES: u64 = 1_000_000;
/// Budget 1: p95 per query class.
const SEARCH_P95_BUDGET_MS: f64 = 50.0;
/// Budget 2: the 1M-files-in-5-minutes red line, scaled to 100k files.
const SCAN_FLOOR_FILES_PER_SEC: f64 = 3300.0;
/// Budget 3: 250MB interned-bytes ceiling at 1M entries (spec: 300MB).
const INTERNED_BUDGET_BYTES: u64 = 250 * 1024 * 1024;

/// The ten query classes of the search budget (same set as
/// benches/search.rs): short prefix, CJK substring, pinyin initials,
/// pinyin prefix, plain substring, a long no-hit query, the modifier
/// combo, a path filter, the empty-query recency mode and a high-hit-count
/// common substring.
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

/// Built once per process (~1-2s) and shared by the search and memory
/// tests (they compile into one test binary).
fn shared_index() -> &'static HotIndex {
    static INDEX: OnceLock<HotIndex> = OnceLock::new();
    INDEX.get_or_init(|| HotIndex::synthetic(INDEX_ENTRIES))
}

/// `pct` of `sorted` (0.0..=1.0), the nearest-rank percentile.
fn percentile(sorted: &[u128], pct: f64) -> u128 {
    let rank = ((sorted.len() as f64) * pct).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// Samples `search_index` on `query` until 200 samples or 6s (always at
/// least 40 samples unless the query is catastrophically over budget —
/// in which case the assert fails anyway). Per-iteration microseconds.
fn sample_query(hot: &HotIndex, query: &str) -> Vec<u128> {
    for _ in 0..3 {
        black_box(search_index(hot, query, 0, 50));
    }
    let mut samples = Vec::new();
    let start = Instant::now();
    while samples.len() < 200 {
        let t = Instant::now();
        black_box(search_index(hot, query, 0, 50));
        samples.push(t.elapsed().as_micros());
        if samples.len() >= 40 && start.elapsed() > Duration::from_secs(6) {
            break;
        }
    }
    samples
}

fn ms(micros: u128) -> f64 {
    micros as f64 / 1000.0
}

/// Budget 1: every query class' p95 must stay under 50ms.
#[test]
#[ignore]
fn perf_1m_search_under_50ms() {
    let hot = shared_index();
    assert!(
        hot.slot_count() >= INDEX_ENTRIES as usize,
        "the synthetic index must hold the budgeted 1M entries"
    );

    println!(
        "{:<18} {:>9} {:>9} {:>9} {:>10} {:>8}",
        "query", "p50(ms)", "p95(ms)", "max(ms)", "samples", "hits"
    );
    let mut over_budget: Vec<(String, f64)> = Vec::new();
    for (name, query) in QUERIES {
        let samples = sample_query(hot, query);
        let mut sorted = samples.clone();
        sorted.sort_unstable();
        let (p50, p95, max) = (
            ms(percentile(&sorted, 0.50)),
            ms(percentile(&sorted, 0.95)),
            ms(sorted[sorted.len() - 1]),
        );
        let hits = search_index(hot, query, 0, 50).total;
        println!(
            "{name:<18} {p50:>9.2} {p95:>9.2} {max:>9.2} {:>10} {hits:>8}",
            samples.len()
        );
        if p95 >= SEARCH_P95_BUDGET_MS {
            over_budget.push((name.to_string(), p95));
        }
    }
    assert!(
        over_budget.is_empty(),
        "query classes over the {SEARCH_P95_BUDGET_MS}ms p95 budget: {over_budget:?}"
    );
}

/// Budget 3: the 1M index must intern under 250MB (the `interned_bytes`
/// metric `Engine::stats` reports).
#[test]
#[ignore]
fn perf_interned_bytes_1m_under_250mb() {
    let hot = shared_index();
    let bytes = hot.interned_bytes();
    let slots = hot.slot_count() as u64;
    let mib = bytes as f64 / (1024.0 * 1024.0);
    println!(
        "interned_bytes @ {slots} slots: {bytes} bytes = {mib:.1} MiB ({:.0} B/entry); budget {INTERNED_BUDGET_BYTES}",
        bytes as f64 / slots as f64
    );
    assert!(slots >= INDEX_ENTRIES);
    assert!(
        bytes < INTERNED_BUDGET_BYTES,
        "interned {mib:.1} MiB misses the {} MiB budget",
        INTERNED_BUDGET_BYTES / (1024 * 1024)
    );
}

/// Receives events until the scan finishes; returns `(files, dirs, elapsed)`.
fn wait_scan_finished(rx: &Receiver<EngineEvent>) -> (u64, u64, Duration) {
    let start = Instant::now();
    loop {
        match rx
            .recv_timeout(Duration::from_secs(600))
            .expect("the scan must finish or fail within 10 minutes")
        {
            EngineEvent::ScanProgress { .. } => continue,
            EngineEvent::ScanFinished { files, dirs } => return (files, dirs, start.elapsed()),
            EngineEvent::ScanFailed { message, .. } => panic!("scan failed: {message}"),
            other => panic!("unexpected event: {other:?}"),
        }
    }
}

/// The 100k scan fixture: 100 dirs x 1000 small files (some CJK-named, so
/// the scan pays the real pinyin-generation cost), written once by 8
/// threads and CACHED in the OS temp dir — regeneration takes minutes,
/// the scan itself must be repeatable. The completion marker lives NEXT TO
/// the fixture root, so a stale partial tree never passes the check and
/// the fixture root holds exactly the 100k indexed files.
fn ensure_scan_fixture() -> PathBuf {
    const DIRS: usize = 100;
    const PER_DIR: usize = 1_000;
    let root = std::env::temp_dir().join("synso-perf-fixture-100k");
    let marker = std::env::temp_dir().join("synso-perf-fixture-100k.complete");
    if marker.exists() && root.is_dir() {
        println!("reusing cached 100k fixture at {}", root.display());
        return root;
    }
    let _ = fs::remove_dir_all(&root);
    println!(
        "generating 100k-file fixture at {} (one-time, cached for later runs)...",
        root.display()
    );
    let start = Instant::now();
    for d in 0..DIRS {
        fs::create_dir_all(root.join(format!("dir{d:03}"))).unwrap();
    }
    // Dirs are partitioned across threads (never two writers per dir), so
    // NTFS directory contention stays off the critical path.
    let content = [0u8; 192];
    thread::scope(|scope| {
        for tid in 0..8usize {
            let root = root.clone();
            scope.spawn(move || {
                for d in (tid..DIRS).step_by(8) {
                    let dir = root.join(format!("dir{d:03}"));
                    for k in 0..PER_DIR {
                        let name = if k % 10 < 7 {
                            format!("file{d:03}_{k:04}.dat")
                        } else {
                            format!("资料{d:03}_{k:04}.pdf")
                        };
                        fs::write(dir.join(name), &content[..1 + (k % 192)]).unwrap();
                    }
                }
            });
        }
    });
    fs::write(&marker, b"ok").unwrap();
    println!("fixture ready in {:.1?}", start.elapsed());
    root
}

/// Budget 2: the first scan of 100k generated files must sustain
/// >= 3300 files/s (the 1M-in-5-minutes red line), timed engine-side.
#[test]
#[ignore]
fn perf_scan_throughput_100k() {
    let fixture = ensure_scan_fixture();
    let db = tempfile::tempdir().unwrap();
    let engine = Engine::open(
        &db.path().join("index.db"),
        EngineOptions {
            volumes: vec![fixture.to_string_lossy().into_owned()],
            exclusions: Vec::new(),
        },
    )
    .unwrap();
    let rx = engine.subscribe();
    engine.start_full_scan();
    let (files, dirs, elapsed) = wait_scan_finished(&rx);
    let rate = files as f64 / elapsed.as_secs_f64();
    println!(
        "first scan: {files} files + {dirs} dir entries in {:.2?} => {rate:.0} files/s (budget >= {SCAN_FLOOR_FILES_PER_SEC})",
        elapsed
    );
    assert_eq!(files, (100 * 1000) as u64, "fixture changed under the test");
    assert!(
        rate >= SCAN_FLOOR_FILES_PER_SEC,
        "scan throughput {rate:.0} files/s misses the {SCAN_FLOOR_FILES_PER_SEC} files/s budget"
    );
}

/// One timed `Engine::search` per query, repeated `rounds` times.
fn sample_engine(engine: &Engine, queries: &[&str], rounds: usize) -> Vec<(&'static str, u128)> {
    let mut samples = Vec::new();
    for _ in 0..rounds {
        for &(name, query) in QUERIES {
            if !queries.contains(&query) {
                continue;
            }
            let t = Instant::now();
            black_box(engine.search(query, 0, 50));
            samples.push((name, t.elapsed().as_micros()));
        }
    }
    samples
}

/// Reports p50/p95 (overall and per query) for one sample batch.
fn report_churn_phase(label: &str, samples: &[(&'static str, u128)]) {
    let mut all: Vec<u128> = samples.iter().map(|(_, micros)| *micros).collect();
    all.sort_unstable();
    println!(
        "{label:<10} n={:<5} overall p50={:>8.2}ms p95={:>8.2}ms max={:>8.2}ms",
        all.len(),
        ms(percentile(&all, 0.50)),
        ms(percentile(&all, 0.95)),
        ms(all[all.len() - 1]),
    );
    for (name, _) in QUERIES {
        let mut per_query: Vec<u128> = samples
            .iter()
            .filter(|(query, _)| query == name)
            .map(|(_, micros)| *micros)
            .collect();
        if per_query.is_empty() {
            continue;
        }
        per_query.sort_unstable();
        println!(
            "{label:<10} {:<18} p50={:>8.2}ms p95={:>8.2}ms",
            name,
            ms(percentile(&per_query, 0.50)),
            ms(percentile(&per_query, 0.95)),
        );
    }
}

/// Observation (no hard assert; Task 17's reader-vs-watcher lock probe):
/// search latency while a watcher applies churn batches. Bursts of 400
/// creates / 400 deletes alternate with 900ms pauses — long enough for the
/// watcher's 500ms quiet window to close and the batch to apply WHILE the
/// query thread keeps sampling — so the sample set straddles idle and
/// batch-under-lock stretches of the run.
#[test]
#[ignore]
fn perf_obs_churn_search_latency() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("fixture");
    for d in 0..3 {
        let dir = root.join(format!("d{d}"));
        fs::create_dir_all(&dir).unwrap();
        for k in 0..1000 {
            fs::write(dir.join(format!("doc{k:04}.txt")), b"churn fixture").unwrap();
        }
    }
    let engine = Engine::open(
        &tmp.path().join("index.db"),
        EngineOptions {
            volumes: vec![root.to_string_lossy().into_owned()],
            exclusions: Vec::new(),
        },
    )
    .unwrap();
    let rx = engine.subscribe();
    engine.start_full_scan();
    let (files, _, _) = wait_scan_finished(&rx);
    assert_eq!(files, 3_000);
    engine.start_watcher().unwrap();

    // Baseline: the index is static here.
    let baseline = sample_engine(&engine, &["report", "doc", "bg", ""], 100);

    // Churn: create/delete bursts in a watched subdirectory while the
    // query thread keeps sampling.
    let churn_dir = root.join("churn");
    fs::create_dir_all(&churn_dir).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_churn = Arc::clone(&stop);
    let churn_root = root;
    let churner = thread::Builder::new()
        .name("synso-perf-churn".into())
        .spawn(move || {
            let churn_dir = churn_root.join("churn");
            let mut i = 0u64;
            let burst = |churn_dir: &std::path::Path, i: &mut u64, create: bool| {
                for _ in 0..400 {
                    let path = churn_dir.join(format!("churn_{}.txt", i));
                    *i += 1;
                    if create {
                        let _ = fs::write(&path, b"churn");
                    } else {
                        let _ = fs::remove_file(&path);
                    }
                }
            };
            while !stop_churn.load(Ordering::Acquire) {
                burst(&churn_dir, &mut i, true);
                thread::sleep(Duration::from_millis(900));
                burst(&churn_dir, &mut i, false);
                thread::sleep(Duration::from_millis(900));
            }
        })
        .unwrap();
    let churn = sample_engine_for(&engine, Duration::from_secs(12));
    stop.store(true, Ordering::Release);
    churner.join().unwrap();

    println!("fixture: 3000 files; churn: 400 create/delete per burst, 900ms pause (watcher quiet window 500ms)");
    report_churn_phase("baseline", &baseline);
    report_churn_phase("churn", &churn);
    // No latency assert: this test OBSERVES lock contention and reports.
    // Settle before yielding the process: the OS's on-access scanning of
    // the ~8000 churned files drains for a while after the last burst, and
    // the alphabetically-next test in this binary (perf_scan_throughput_100k)
    // owns a hard throughput assert that the drain can break. Observed as a
    // one-off flake before this sleep existed.
    thread::sleep(Duration::from_secs(2));
}

/// Like [`sample_engine`], but bounded by wall time instead of rounds.
fn sample_engine_for(engine: &Engine, budget: Duration) -> Vec<(&'static str, u128)> {
    let mut samples = Vec::new();
    let start = Instant::now();
    while start.elapsed() < budget {
        for &(name, query) in QUERIES {
            let t = Instant::now();
            black_box(engine.search(query, 0, 50));
            samples.push((name, t.elapsed().as_micros()));
        }
    }
    samples
}
