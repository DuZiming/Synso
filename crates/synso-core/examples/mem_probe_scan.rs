//! Scan-path memory probe (v1.1 Task 6): like `mem_probe`, but it also
//! RUNS a full scan over the DB copy and samples the process working set
//! throughout — load, scan peak, and post-scan settle — answering the
//! "why is the app at 1GB when a pure load is ~480MB" question: how much
//! of the gap is the rescan's transient high water, and whether RSS falls
//! back once the scan finishes.
//!
//! Run: `cargo run --release -p synso-core --example mem_probe_scan -- <db>`

use std::time::{Duration, Instant};

use synso_core::{Engine, EngineEvent, EngineOptions};

fn main() {
    let mut args = std::env::args().skip(1);
    let db = args.next().expect("usage: mem_probe_scan <db> [dedup]");
    let dedup_mode = args.next().as_deref() == Some("dedup");
    let before = working_set_kb();
    let engine = Engine::open(std::path::Path::new(&db), EngineOptions::default()).expect("open");
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
