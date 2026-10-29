//! Recycle-bin deletion (Task 10): the ONLY way Synso removes user files,
//! and always to the OS trash — a permanent delete never happens (no
//! `std::fs::remove_*` anywhere in this feature; only the trash crate's
//! move-to-trash runs against user data). Two layers:
//!
//! * [`delete_paths_to_trash`] — the store-free, engine-free batch over the
//!   `trash` crate, unit-testable on its own.
//! * [`crate::engine::Engine::delete_to_trash`] — resolves ids to live hits
//!   and absolute disk paths, hands the batch here, then syncs the store
//!   and the hot index per path that actually left the disk.
//!
//! Failure semantics: a refused path is never fatal. The batch goes out as
//! ONE [`trash::delete_all`] call, but platform backends differ in how a
//! failing batch behaves (some stop at the first refusing item), so success
//! is counted from disk state — present before the call, absent after —
//! instead of from the crate's all-or-nothing `Result`. Whatever the
//! outcome, the next rescan rebuilds each root from disk truth and
//! self-heals any residual index drift.

use std::path::Path;

use crate::Result;

/// Moves `paths` to the OS trash (directories included — the trash crate
/// handles whole directories) and returns how many of them are gone from
/// disk afterwards. One batch call for the whole slice. A path missing
/// before the call counts as neither success nor failure, and a path the
/// backend refuses is logged and skipped. Always `Ok` — even when nothing
/// (or not everything) landed; the `Result` keeps the engine's calling
/// convention open for fatal pre-check failures. Never permanently deletes.
pub fn delete_paths_to_trash(paths: &[&Path]) -> Result<usize> {
    if paths.is_empty() {
        return Ok(0);
    }
    let was_present: Vec<bool> = paths.iter().map(|path| present(path)).collect();
    if let Err(err) = trash::delete_all(paths) {
        // Not fatal: whatever the batch did land is counted below from disk
        // state; the refused paths keep living where they are.
        log::warn!("synso: batch trash failed: {err}");
    }
    Ok(paths
        .iter()
        .zip(was_present)
        .filter(|(path, was)| *was && !present(path))
        .count())
}

/// True while `path` is a filesystem node of any kind — `symlink_metadata`,
/// so a symlink counts by the link itself, not by its target (the same
/// basis the watcher's demote-to-removal stat uses).
pub(crate) fn present(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_paths_moves_files_and_dirs_to_trash() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("gone.txt");
        std::fs::write(&file, b"bye").unwrap();
        let dir = tmp.path().join("gone_dir");
        std::fs::create_dir_all(dir.join("inner")).unwrap();
        std::fs::write(dir.join("inner").join("child.txt"), b"c").unwrap();

        let count = delete_paths_to_trash(&[file.as_path(), dir.as_path()]).unwrap();
        assert_eq!(count, 2, "both the file and the directory count");
        assert!(!present(&file));
        assert!(!present(&dir));
    }

    #[test]
    fn delete_paths_empty_input_is_zero() {
        assert_eq!(delete_paths_to_trash(&[]).unwrap(), 0);
    }
}
