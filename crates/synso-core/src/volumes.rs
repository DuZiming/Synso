//! Fixed local volume enumeration, uniform across platforms via `sysinfo`.
//!
//! v1 indexes fixed local volumes only, so removable media (USB sticks and
//! the like) are filtered out; see the design doc's non-goals.

use sysinfo::{Disk, Disks};

/// One fixed local volume (removable media excluded).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct VolumeInfo {
    /// Mount point, ready to use as a scan root (e.g. `C:\` on Windows,
    /// `/` on unix).
    pub path: String,
    /// Human-readable label: the volume label when the OS exposes one,
    /// otherwise a mount-point-derived name.
    pub label: String,
    /// Total capacity in bytes.
    pub total_bytes: u64,
    /// Free space in bytes (OS-reported available). With `total_bytes`
    /// this yields the physical used figure the capacity map overlays
    /// at its volume ring (v1.1).
    pub available_bytes: u64,
}

/// All fixed (non-removable) local volumes, sorted by mount path.
pub fn fixed_volumes() -> Vec<VolumeInfo> {
    let disks = Disks::new_with_refreshed_list();
    let mut volumes: Vec<VolumeInfo> = disks
        .list()
        .iter()
        .filter(|disk| !disk.is_removable())
        .map(|disk| VolumeInfo {
            path: disk.mount_point().to_string_lossy().into_owned(),
            label: volume_label(disk),
            total_bytes: disk.total_space(),
            available_bytes: disk.available_space(),
        })
        .collect();
    volumes.sort_by(|a, b| a.path.cmp(&b.path));
    volumes
}

/// Whether `root` names exactly the volume behind one of `volumes` — the
/// ONLY pane level that gets the physical-capacity overlay. The index
/// spells a volume root as its last path segment (`C`), sysinfo as the
/// mount point (`C:\`), users type either — so both sides are trimmed of
/// trailing separators and compared ASCII case-insensitively. A
/// subdirectory, a custom scan root, and the empty "(all)" root never
/// name a whole volume.
pub fn match_volume<'a>(root: &str, volumes: &'a [VolumeInfo]) -> Option<&'a VolumeInfo> {
    let key = normalize_root(root);
    if key.is_empty() {
        return None;
    }
    volumes.iter().find(|v| normalize_root(&v.path) == key)
}

/// `root` with trailing `/` and `\` removed and folded to ASCII
/// lowercase; empty input stays empty (the caller treats that as no
/// match).
fn normalize_root(root: &str) -> String {
    root.trim_end_matches(['/', '\\']).to_ascii_lowercase()
}

/// Label preferring the volume/device name sysinfo reports (the volume
/// label on Windows), falling back to a mount-point-derived name so the
/// label is never empty.
fn volume_label(disk: &Disk) -> String {
    let label = disk.name().to_string_lossy().trim().to_owned();
    if !label.is_empty() {
        return label;
    }
    let mount = disk.mount_point();
    mount
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| mount.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_volumes_lists_fixed_disks() {
        let volumes = fixed_volumes();

        let paths: Vec<&str> = volumes.iter().map(|v| v.path.as_str()).collect();
        assert!(
            paths.iter().all(|p| !p.is_empty()),
            "mount points must be non-empty: {paths:?}"
        );
        let mut unique = paths.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), paths.len(), "mount points must be unique");

        // The test host always has at least one fixed volume with capacity;
        // the label fallback guarantees a non-empty label.
        #[cfg(windows)]
        {
            assert!(!volumes.is_empty(), "host has at least one fixed volume");
            assert!(volumes.iter().all(|v| v.total_bytes > 0));
            assert!(volumes.iter().all(|v| !v.label.is_empty()));
        }
    }

    #[test]
    fn volume_reports_available_space() {
        let volumes = fixed_volumes();

        #[cfg(windows)]
        {
            assert!(!volumes.is_empty(), "host has at least one fixed volume");
            for v in &volumes {
                assert!(
                    v.available_bytes > 0 && v.available_bytes <= v.total_bytes,
                    "0 < available <= total must hold: {v:?}"
                );
            }
        }
    }
}
