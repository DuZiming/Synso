//! Scan-path memory probe (v1.1 Task 6): like `mem_probe`, but it also
//! RUNS a full scan over the DB copy and samples the process working set
//! throughout — load, scan peak, and post-scan settle — answering the
//! "why is the app at 1GB when a pure load is ~480MB" question: how much
//! of the gap is the rescan's transient high water, and whether RSS falls
//! back once the scan finishes.
//!
//! Modes:
//! - `<db> [dedup]` — the memory probe above.
//! - `dump <db> [out.tsv]` — run one full dedup pass over the given DB,
//!   then page through every group and write a baseline TSV: one line per
//!   member (`hash\tsize\tid\tpath`), groups separated by blank lines, a
//!   final `# groups: N` line. Status/progress goes to stderr so stdout
//!   stays parseable when the TSV targets it. This is the comparison
//!   baseline of the dedup redesign: Task 12 runs the pre-change exe and
//!   the new build back-to-back over the same DB copy and diffs the dumps.
//!   Accepts both `dump <db>` and `<db> dump` — the plan's Task 12 spells
//!   it db-first.
//! - `selftest` — build a tiny fixture in a temp dir (`a/x.txt` +
//!   `b/x.txt` with identical 10-byte content, `a/y.bin` with a unique
//!   size), scan + dedup it through a real engine, and assert exactly one
//!   group of two members at size 10. This IS the example's test: any
//!   mismatch exits non-zero with a clear message.
//!
//! Run: `cargo run --release -p synso-core --example mem_probe_scan -- <db>`
//! Run: `cargo run --release -p synso-core --example mem_probe_scan -- dump <db> [out.tsv]`
//! Run: `cargo run --release -p synso-core --example mem_probe_scan -- selftest`

use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, RecvTimeoutError};

use synso_core::{Engine, EngineEvent, EngineOptions};

/// Page size for `dedup_groups` paging: small enough that only one chunk
/// of materialized groups is alive at a time, large enough that a huge
/// baseline doesn't turn into millions of round-trips.
const PAGE: u32 = 500;
/// A dedup run that emits no event for this long counts as stalled
/// (progress fires at every stage change and every 500 records within one).
const EVENT_TIMEOUT: Duration = Duration::from_secs(600);
/// Same stall guard for the selftest's tiny fixture scan.
const SCAN_TIMEOUT: Duration = Duration::from_secs(120);

fn main() {
    let mut args = std::env::args().skip(1);
    let first = args.next();
    let second = args.next();
    let third = args.next();
    match (first.as_deref(), second.as_deref()) {
        (Some("selftest"), _) => selftest(),
        // The plan's Task 12 drives dump db-first (`mem_probe_scan <db>
        // dump`), the Task 1 spec spells it `dump <db>` — accept both so
        // either spelling runs the dump instead of falling into the probe.
        (Some("dump"), db) => {
            dump(db.expect("usage: mem_probe_scan dump <db> [out.tsv]"), third)
        }
        (Some(db), Some("dump")) => dump(db, third),
        (Some(db), dedup_flag) => probe(db, dedup_flag == Some("dedup")),
        (None, _) => {
            panic!("usage: mem_probe_scan <db> [dedup] | [dump] <db> [out.tsv] | selftest")
        }
    }
}

/// The original memory-probe mode, unchanged.
fn probe(db: &str, dedup_mode: bool) {
    let before = working_set_kb();
    let engine = Engine::open(Path::new(db), EngineOptions::default()).expect("open");
    wait_warm(&engine);
    let loaded = working_set_kb();
    println!(
        "after load: {} MB (baseline {} MB), files {}",
        loaded as f64 / 1024.0,
        before as f64 / 1024.0,
        engine.stats().files
    );

    let events = engine.subscribe();
    let started = Instant::now();
    if dedup_mode {
        engine.dedup_start();
    } else {
        engine.start_full_scan();
    }
    let mut peak = loaded;
    let mut last_sample = Instant::now() - Duration::from_secs(10);
    loop {
        // Poll events non-blockingly; sample the working set every 10s.
        if last_sample.elapsed() >= Duration::from_secs(10) {
            let ws = working_set_kb();
            peak = peak.max(ws);
            println!(
                "run +{}s: ws {} MB (peak {} MB)",
                started.elapsed().as_secs(),
                ws as f64 / 1024.0,
                peak as f64 / 1024.0,
            );
            last_sample = Instant::now();
        }
        match events.recv_timeout(Duration::from_millis(200)) {
            Ok(EngineEvent::ScanProgress { done, .. }) => {
                print!("\r  walked {done} files…");
                use std::io::Write;
                let _ = std::io::stdout().flush();
            }
            Ok(EngineEvent::ScanFinished { files, dirs }) => {
                println!("\nscan finished: {files} files, {dirs} dirs");
                break;
            }
            Ok(EngineEvent::DedupFinished { groups }) => {
                println!("\ndedup finished: {groups} groups");
                break;
            }
            Ok(EngineEvent::ScanFailed { message, .. }) => {
                println!("\nscan FAILED: {message}");
                std::process::exit(2);
            }
            Ok(_) => {}
            Err(_) => continue,
        }
    }
    println!("run peak ws: {} MB", peak as f64 / 1024.0);

    // The settle question: 3 minutes of idle after the scan — does RSS
    // come back down, or does the allocator keep the high water?
    let settle_start = Instant::now();
    let mut settle_min = peak;
    while settle_start.elapsed() < Duration::from_secs(180) {
        std::thread::sleep(Duration::from_secs(10));
        let ws = working_set_kb();
        settle_min = settle_min.min(ws);
        println!(
            "settle +{}s: ws {} MB",
            settle_start.elapsed().as_secs(),
            ws as f64 / 1024.0
        );
    }
    println!(
        "post-run settle floor: {} MB (load was {} MB, peak {} MB)",
        settle_min as f64 / 1024.0,
        loaded as f64 / 1024.0,
        peak as f64 / 1024.0,
    );
}

/// Dump mode: one full dedup pass over the given DB (default volumes, like
/// the app — the DB copy's roots map back onto the real fixed volumes),
/// then every group paged out as TSV.
fn dump(db: &str, out: Option<String>) {
    let engine = Engine::open(Path::new(db), EngineOptions::default()).expect("open");
    wait_warm(&engine);
    let events = engine.subscribe();
    let started = Instant::now();
    engine.dedup_start();
    let announced = wait_dedup_done(&events);
    eprintln!(
        "dedup finished: {announced} groups in {}s",
        started.elapsed().as_secs()
    );

    let stdout = std::io::stdout();
    let mut writer: Box<dyn Write> = match out.as_deref() {
        Some(path) => Box::new(BufWriter::new(
            fs::File::create(path).unwrap_or_else(|err| panic!("create {path}: {err}")),
        )),
        None => Box::new(BufWriter::new(stdout.lock())),
    };

    // Page through the materialized groups. The announced count above is
    // the raw run's; this loop's count is what the current index still
    // serves (files deleted since the run drop out, dissolved groups go).
    let mut dumped = 0u64;
    let mut offset = 0u32;
    loop {
        let page = engine.dedup_groups(offset, PAGE);
        if page.is_empty() {
            break;
        }
        for group in &page {
            if dumped > 0 {
                writeln!(&mut writer).expect("write blank separator");
            }
            for member in &group.members {
                writeln!(
                    &mut writer,
                    "{}\t{}\t{}\t{}",
                    group.hash, group.size, member.id, member.path
                )
                .expect("write member line");
            }
            dumped += 1;
        }
        offset += page.len() as u32;
        if page.len() < PAGE as usize {
            break;
        }
    }
    writeln!(&mut writer, "# groups: {dumped}").expect("write summary");
    writer.flush().expect("flush dump");
    if let Some(path) = out.as_deref() {
        eprintln!("wrote {dumped} groups to {path}");
    }
}

/// Selftest mode: the fixture → scan → dedup → assert pipeline, end to
/// end, against a real engine on a throwaway DB. Exit codes: 1 assertion
/// mismatch / scan failed, 3 stalled or dropped event stream.
fn selftest() {
    let base = fixture_base();
    write_fixture(&base);
    let engine = Engine::open(
        &base.join("selftest.db"),
        EngineOptions {
            volumes: vec![base.to_string_lossy().into_owned()],
            exclusions: Vec::new(),
        },
    )
    .expect("open selftest db");
    wait_warm(&engine);

    let events = engine.subscribe();
    engine.start_full_scan();
    wait_scan_done(&events);
    engine.dedup_start();
    let announced = wait_dedup_done(&events);

    // Page the materialized groups exactly the way `dump` does.
    let mut groups = Vec::new();
    let mut offset = 0u32;
    loop {
        let page = engine.dedup_groups(offset, PAGE);
        if page.is_empty() {
            break;
        }
        let got = page.len();
        groups.extend(page);
        offset += got as u32;
        if got < PAGE as usize {
            break;
        }
    }

    let mut ok = true;
    if groups.len() != 1 {
        eprintln!(
            "selftest FAILED: expected exactly 1 group, got {}",
            groups.len()
        );
        ok = false;
    }
    if let Some(group) = groups.first() {
        if group.members.len() != 2 {
            eprintln!(
                "selftest FAILED: expected 2 members, got {}",
                group.members.len()
            );
            ok = false;
        }
        if group.size != 10 {
            eprintln!("selftest FAILED: expected size 10, got {}", group.size);
            ok = false;
        }
    }
    if !ok {
        std::process::exit(1);
    }
    println!("selftest OK: 1 group, 2 members, size 10 (run announced {announced})");

    // The watcher / heat threads may still hold handles on Windows; the
    // cleanup is best-effort — leftovers land in %TEMP% and are harmless.
    drop(engine);
    let _ = fs::remove_dir_all(&base);
}

/// Unique scratch dir for the selftest fixture (`std` only — the example
/// stays dependency-free).
fn fixture_base() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "synso-probe-selftest-{}-{}",
        std::process::id(),
        nanos
    ))
}

/// The fixture: `a/x.txt` + `b/x.txt` share 10 identical bytes (one
/// expected group of two members at size 10), `a/y.bin` carries a size no
/// other fixture file has (the size-class gate must keep it ungrouped).
fn write_fixture(base: &Path) {
    fs::create_dir_all(base.join("a")).expect("mkdir a");
    fs::create_dir_all(base.join("b")).expect("mkdir b");
    fs::write(base.join("a/x.txt"), b"0123456789").expect("write a/x.txt");
    fs::write(base.join("b/x.txt"), b"0123456789").expect("write b/x.txt");
    fs::write(base.join("a/y.bin"), vec![b'y'; 13]).expect("write a/y.bin");
}

/// Waits for the fixture scan's `ScanFinished`; a `ScanFailed` or a stalled
/// event stream exits non-zero.
fn wait_scan_done(events: &Receiver<EngineEvent>) -> (u64, u64) {
    loop {
        match events.recv_timeout(SCAN_TIMEOUT) {
            Ok(EngineEvent::ScanFinished { files, dirs }) => return (files, dirs),
            Ok(EngineEvent::ScanFailed { message, .. }) => {
                eprintln!("selftest FAILED: scan failed: {message}");
                std::process::exit(1);
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {
                eprintln!(
                    "selftest FAILED: scan emitted no event for {}s",
                    SCAN_TIMEOUT.as_secs()
                );
                std::process::exit(3);
            }
            Err(RecvTimeoutError::Disconnected) => {
                eprintln!("selftest FAILED: engine dropped before ScanFinished");
                std::process::exit(3);
            }
        }
    }
}

/// Waits for a dedup run's `DedupFinished` — the mem-probe loop's event
/// handling, narrowed to the dedup events (progress goes to stderr so a
/// TSV on stdout stays parseable). Returns the run's announced group count.
fn wait_dedup_done(events: &Receiver<EngineEvent>) -> u64 {
    let started = Instant::now();
    loop {
        match events.recv_timeout(EVENT_TIMEOUT) {
            Ok(EngineEvent::DedupProgress { phase, done, total }) => {
                eprintln!(
                    "dedup {phase:?}: {done}/{total} (+{}s)",
                    started.elapsed().as_secs()
                );
            }
            Ok(EngineEvent::DedupFinished { groups }) => return groups,
            Ok(EngineEvent::ScanFailed { message, .. }) => {
                eprintln!("FAILED: scan failed while waiting for dedup: {message}");
                std::process::exit(1);
            }
            Ok(_) => {} // watcher/fs events alongside a running dedup
            Err(RecvTimeoutError::Timeout) => {
                eprintln!(
                    "FAILED: dedup emitted no event for {}s — stalled",
                    EVENT_TIMEOUT.as_secs()
                );
                std::process::exit(3);
            }
            Err(RecvTimeoutError::Disconnected) => {
                eprintln!("FAILED: engine dropped before DedupFinished");
                std::process::exit(3);
            }
        }
    }
}

fn wait_warm(engine: &Engine) {
    let deadline = Instant::now() + Duration::from_secs(300);
    while !engine.stats().warm {
        if Instant::now() > deadline {
            panic!("hot index load did not finish in 300s");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Process working set in KB via a Get-Process shell-out (same approach
/// as `mem_probe`; keeps the example dependency-free).
#[cfg(windows)]
fn working_set_kb() -> u64 {
    let cmd = format!("(Get-Process -Id {}).WorkingSet64", std::process::id());
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &cmd])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse()
                .unwrap_or(0)
                / 1024
        }
        _ => 0,
    }
}

#[cfg(not(windows))]
fn working_set_kb() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let resident_pages: u64 = statm
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    resident_pages * 4
}
