//! One-off `path:` filter diagnosis (2026-11-05): opens a db copy, waits
//! warm, and compares searches with and without `path:` filters, printing
//! totals and sample paths so filter behavior is observable end-to-end.
//! Usage: cargo run --release -p synso-core --example path_probe -- <db>

use synso_core::{Engine, EngineOptions};

fn main() {
    let db = std::env::args()
        .nth(1)
        .expect("usage: path_probe <path-to-db-copy>");
    let engine = Engine::open(
        std::path::Path::new(&db),
        EngineOptions {
            volumes: Vec::new(),
            exclusions: Vec::new(),
        },
    )
    .unwrap();
    // Wait for the background load.
    loop {
        let s = engine.stats();
        if s.warm {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    println!("warm: {:?}", engine.stats());
    for q in [
        "synso",
        "path:Desktop synso",
        "path:desktop synso",
        "path:Desktop\\Synso synso",
        "path:target synso",
        "path:Windows synso",
        "path:AppData synso",
        "path:zzz_no_such_dir synso",
        "path:Windows",
    ] {
        let r = engine.search(q, 0, 5);
        println!("\nquery {q:?} → total={}", r.total);
        for h in &r.hits {
            println!("   {}", h.path);
        }
    }
}
