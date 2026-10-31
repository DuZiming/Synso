//! Periodic process-memory sampling (v1.1 Task 6): every
//! [`SAMPLE_INTERVAL`] one log line carries the OS view (working set,
//! commit) next to the engine's own stats, so a memory report from the
//! field can be attributed — engine structures vs. process overhead vs.
//! scan-path high water — straight from the existing file log.
//!
//! Windows note: `sysinfo`'s process refresh reports the WORKING SET and
//! COMMITTED bytes of THIS process only; the WebView2 child processes
//! (separate PIDs) are deliberately excluded — the engine budget never
//! counted them.

use std::thread;
use std::time::Duration;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use std::sync::Arc;

/// How often a sample lands in the log.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(60);

/// Spawns the sampler thread; one line per [`SAMPLE_INTERVAL`] from app
/// start for the whole process lifetime. Best-effort by design: a sysinfo
/// failure logs the error once per occurrence and the next tick retries.
pub fn spawn(engine: Arc<synso_core::Engine>) {
    thread::Builder::new()
        .name("synso-memlog".into())
        .spawn(move || {
            let pid = Pid::from_u32(std::process::id());
            let mut system = System::new();
            loop {
                thread::sleep(SAMPLE_INTERVAL);
                system.refresh_processes_specifics(
                    ProcessesToUpdate::Some(&[pid]),
                    true,
                    ProcessRefreshKind::everything(),
                );
                let Some(process) = system.process(pid) else {
                    log::warn!("synso: mem sampler lost its own process entry");
                    continue;
                };
                let stats = engine.stats();
                log::info!(
                    "mem: ws={:.0}MB commit={:.0}MB files={} dirs={} interned={:.1}MB",
                    process.memory() as f64 / 1e6,
                    process.virtual_memory() as f64 / 1e6,
                    stats.files,
                    stats.dirs,
                    stats.interned_bytes as f64 / 1e6,
                );
                // Per-structure breakdown (v1.1 diagnostics): the
                // live-bytes estimate next to the working set — the gap
                // between the two IS allocator overhead + high water.
                let b = engine.mem_breakdown();
                log::info!(
                    "mem breakdown: live≈{:.0}MB | arena {:.0} + refs {:.0} + cols {:.0} + slot_by_id {:.0} + slot_by_name {:.0} + dir_tree {:.0} + dir_index {:.0} + exts {:.0} | slots={} dirs={}",
                    b.total_estimated as f64 / 1e6,
                    b.arena_bytes as f64 / 1e6,
                    b.slot_refs_bytes as f64 / 1e6,
                    b.slot_cols_bytes as f64 / 1e6,
                    b.slot_by_id_bytes as f64 / 1e6,
                    b.slot_by_parent_name_bytes as f64 / 1e6,
                    b.dir_tree_bytes as f64 / 1e6,
                    b.dir_index_bytes as f64 / 1e6,
                    b.exts_bytes as f64 / 1e6,
                    b.slots,
                    b.dir_rows,
                );
            }
        })
        .inspect_err(|err| log::warn!("synso: mem sampler failed to spawn: {err}"))
        .ok();
}
