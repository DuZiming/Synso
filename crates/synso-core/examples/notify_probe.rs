//! Standalone notify probe (2026-11-05 diagnosis): watches C:\ recursively
//! with the workspace's notify version and prints Desktop events with
//! arrival timestamps. Usage: run it, then create/modify a file on the
//! Desktop; compare wall-clock. If events arrive minutes late HERE, the
//! delay lives in the OS/notify layer (antivirus, buffer), not in Synso.

use notify::{recommended_watcher, RecursiveMode, Watcher};
use std::path::Path;
use std::time::{Duration, SystemTime};

fn main() {
    let root = std::env::args().nth(1).unwrap_or_else(|| "C:\\".into());
    let root = Path::new(&root).to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel::<notify::Result<notify::Event>>();
    let mut w = recommended_watcher(move |ev: notify::Result<notify::Event>| {
        let _ = tx.send(ev);
    })
    .unwrap();
    w.watch(&root, RecursiveMode::Recursive).unwrap();
    println!(
        "watching {root:?} — create/modify a Desktop file now ({:?})",
        SystemTime::now()
    );
    loop {
        match rx.recv() {
            Ok(Ok(ev)) => {
                for p in &ev.paths {
                    let s = p.to_string_lossy();
                    if s.contains("Desktop") || s.contains("synso") {
                        println!("[{:?}] {:?} {}", SystemTime::now(), ev.kind, s);
                    }
                }
            }
            Ok(Err(e)) => println!("[{:?}] notify error: {e}", SystemTime::now()),
            Err(e) => {
                println!("channel closed: {e}");
                return;
            }
        }
    }
}
