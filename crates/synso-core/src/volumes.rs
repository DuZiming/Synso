//! Fixed local volume enumeration, uniform across platforms via `sysinfo`.
//!
//! v1 indexes fixed local volumes only, so removable media (USB sticks and
//! the like) are filtered out; see the design doc's non-goals.

use sysinfo::{Disk, Disks};

/// One fixed local volume (removable media excluded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeInfo {
    /// Mount point, ready to use as a scan root (e.g. `C:\` on Windows,
    /// `/` on unix).
    pub path: String,
    /// Human-readable label: the volume label when the OS exposes one,
    /// otherwise a mount-point-derived name.
    pub label: String,
    /// Total capacity in bytes.
    pub total_bytes: u64,
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
        })
        .collect();
    volumes.sort_by(|a, b| a.path.cmp(&b.path));
    volumes
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
}
