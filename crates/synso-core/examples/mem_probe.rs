//! Memory probe: loads a real index database into the hot index and
//! reports the TRUE process footprint — interned content vs. working set.
//! The stats tile's `interned_bytes` counts arena string content only and
//! badly under-reports RSS (indexes, per-String allocator overhead, and
//! fragmentation are invisible to it); this example measures what the OS
//! actually charges.
//!
//! Run: `cargo run --release -p synso-core --example mem_probe -- <db>`
//! (a COPY of the live db — never the file a running app holds).

use synso_core::Engine;

fn main() {
    let db = std::env::args()
        .nth(1)
        .expect("usage: mem_probe <path-to-db-copy>");
    let before = working_set_kb();
    let engine = Engine::open(std::path::Path::new(&db), Default::default()).expect("open");
    // Wait for the startup hot-index load to finish.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while !engine.stats().warm {
        if std::time::Instant::now() > deadline {
            panic!("hot index load did not finish in 120s");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let stats = engine.stats();
    let after = working_set_kb();
    println!("files: {}", stats.files);
    println!("dirs: {}", stats.dirs);
    println!(
        "interned_bytes: {:.1} MB",
        stats.interned_bytes as f64 / 1e6
    );
    println!(
        "working_set: {:.0} MB (was {:.0} MB before load)",
        after as f64 / 1024.0,
        before as f64 / 1024.0
    );
    println!(
        "overhead ratio: {:.2}x content",
        after.saturating_sub(before) as f64 / stats.interned_bytes.max(1) as f64
    );
}

/// Process working set in KB, read from a Get-Command shell-out on
/// Windows (the windows crate's process-status features are not enabled
/// for the core; a shell-out keeps the example dependency-free). The
/// child PowerShell reports OUR pid, not its own.
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
    // Non-Windows hosts: /proc/self/statm resident pages.
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let resident_pages: u64 = statm
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    resident_pages * 4
}
