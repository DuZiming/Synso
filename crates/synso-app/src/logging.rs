//! Local-only file logging (Task 19).
//!
//! A hand-rolled [`log`] backend writing to `<app_data_dir>/logs/synso.log`
//! with size rotation: 5 MB per file, 3 files kept (`synso.log` current,
//! `synso.log.1` / `synso.log.2` the two previous generations).
//!
//! Why hand-rolled instead of `fern`: the rotation contract is exactly
//! "size trigger, N kept files" — a fixed ~150-line `Log` impl over
//! `std::fs` covers it with zero new dependencies (the engine already
//! pulls in `log`; a backend crate would add a dependency for what is
//! here a rename chain). Windows cannot rename an open file, so rotation
//! closes the current handle first and reopens after the shift; all
//! writes serialize behind one mutex, which also makes the size check and
//! the rotation atomic.
//!
//! Levels: `debug` in debug builds (mirrored to stdout for `cargo run`),
//! `info` in release. Failure semantics: logging never fails the app —
//! an unopenable log file leaves the logger installed with no sink (plus
//! the stdout mirror in debug), and every write is best-effort.
//!
//! Everything stays on disk: no network sink exists, and the only
//! consumers are this process's `log` macros — the engine's
//! `log::error!`/`log::warn!` calls (the Task 12 swap) land here too.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use log::{LevelFilter, Log, Metadata, Record};

/// Rotation trigger: a log file grows to this many bytes before the
/// generation shift.
pub const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;

/// Files kept: the current log plus this many minus one archived
/// generations (`synso.log`, `.1`, `.2` for 3).
pub const MAX_FILES: usize = 3;

/// The file backend. `file` is `None` while the sink is closed (open
/// failure, or between a rotation's close and reopen) — writes then go
/// nowhere instead of failing the caller.
pub struct FileLogger {
    path: PathBuf,
    max_bytes: u64,
    max_files: usize,
    mirror_stdout: bool,
    file: Mutex<Option<File>>,
}

impl FileLogger {
    fn new(path: PathBuf, max_bytes: u64, max_files: usize, mirror_stdout: bool) -> Self {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok();
        FileLogger {
            path,
            max_bytes,
            max_files,
            mirror_stdout,
            file: Mutex::new(file),
        }
    }

    /// Appends one formatted line, rotating first when the current file
    /// has reached [`MAX_FILE_BYTES`]. Best-effort throughout: a failed
    /// stat/rotate/write loses that line, never the app.
    fn write_line(&self, line: &str) {
        let mut slot = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let size = std::fs::metadata(&self.path).map_or(0, |meta| meta.len());
        if size >= self.max_bytes {
            self.rotate(&mut slot);
        }
        if let Some(file) = slot.as_mut() {
            let _ = file.write_all(line.as_bytes());
        }
        if self.mirror_stdout {
            let _ = std::io::stdout().write_all(line.as_bytes());
        }
    }

    /// The generation shift. CALLER holds the file mutex and the current
    /// handle is about to be replaced: close (drop) first — Windows cannot
    /// rename an open file — then `delete .2; rename .1 -> .2; rename log
    /// -> .1`, then reopen. Every step tolerates a missing predecessor
    /// (first rotation: no `.1`/`.2` exist yet).
    fn rotate(&self, slot: &mut Option<File>) {
        *slot = None;
        let ext = |gen: usize| -> PathBuf {
            let mut path = self.path.clone().into_os_string();
            path.push(format!(".{gen}"));
            PathBuf::from(path)
        };
        let _ = std::fs::remove_file(ext(self.max_files - 1));
        for gen in (1..self.max_files - 1).rev() {
            let _ = std::fs::rename(ext(gen), ext(gen + 1));
        }
        let _ = std::fs::rename(&self.path, ext(1));
        *slot = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .ok();
    }
}

impl Log for FileLogger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        // The global max level (set in [`init`]) does the filtering.
        true
    }

    fn log(&self, record: &Record) {
        let line = format!(
            "[{}] [{:<5}] [{}] {}\n",
            timestamp_now(),
            record.level(),
            record.target(),
            record.args()
        );
        self.write_line(&line);
    }

    fn flush(&self) {
        if let Some(file) = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
        {
            let _ = file.flush();
        }
    }
}

/// Installs the backend. `log_dir` is created on demand; the level is
/// `debug` in debug builds (with a stdout mirror for `cargo run`) and
/// `info` in release. Called first in the Tauri setup hook, so everything
/// after it — engine open, watcher, tray — is captured.
pub fn init(log_dir: &Path) {
    let _ = std::fs::create_dir_all(log_dir);
    let logger = FileLogger::new(
        log_dir.join("synso.log"),
        MAX_FILE_BYTES,
        MAX_FILES,
        cfg!(debug_assertions),
    );
    let level = if cfg!(debug_assertions) {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    };
    // A second init (tests, re-entry) keeps the first backend; that first
    // backend is then the sink, which is the correct outcome either way.
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(level);
    }
    log::info!(
        "synso: file logging started at {} (level {level})",
        log_dir.join("synso.log").display()
    );
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` (UTC) for the log prefix. Hand-rolled off
/// the Unix epoch (civil-from-days, Hinnant's algorithm) so no
/// date library is pulled in for a log line.
fn timestamp_now() -> String {
    let (secs, millis) = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(delta) => (delta.as_secs() as i64, delta.subsec_millis()),
        Err(_) => (0, 0),
    };
    format!("{}.{:03}Z", format_utc_seconds(secs), millis)
}

/// Renders Unix-epoch seconds as `YYYY-MM-DDTHH:MM:SSZ`.
fn format_utc_seconds(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (hour, min, sec) = (sod / 3_600, (sod % 3_600) / 60, sod % 60);

    // Howard Hinnant's civil_from_days: valid for the full i64 day range.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if month <= 2 { year + 1 } else { year };

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Civil-date conversion spot checks: the epoch, a leap day, and a
    /// known recent midnight.
    #[test]
    fn utc_formatting_known_instants() {
        assert_eq!(format_utc_seconds(0), "1970-01-01T00:00:00");
        // 2000-02-29 (leap day, divisible by 400 era math).
        assert_eq!(format_utc_seconds(951_782_400), "2000-02-29T00:00:00");
        assert_eq!(format_utc_seconds(1_751_241_600), "2025-06-30T00:00:00");
        // 23:59:59 same day: carries into the next day only on the second.
        assert_eq!(format_utc_seconds(1_751_241_599), "2025-06-29T23:59:59");
    }

    /// The rotation contract at toy scale: 64-byte files, 3 kept. The
    /// size check runs BEFORE each write, so a file crosses the limit and
    /// rotates on the NEXT line. Writing on must (a) start a fresh
    /// current file, (b) shift the generations down, and (c) never keep
    /// more than [`MAX_FILES`] files — the oldest content is gone once a
    /// fourth generation starts.
    #[test]
    fn rotation_keeps_three_generations_and_drops_the_oldest() {
        fn line(gen: usize, ch: char) -> String {
            // Exactly 30 bytes: 11 (prefix) + 18 (filler) + 1 (newline).
            // Three lines fill past the 64-byte limit; two do not.
            format!(
                "gen-{gen}-line-{0}{0}{0}{0}{0}{0}{0}{0}{0}{0}{0}{0}{0}{0}{0}{0}{0}{0}\n",
                ch
            )
        }

        let tmp = tempfile::tempdir().unwrap();
        let logger = FileLogger::new(tmp.path().join("synso.log"), 64, 3, false);

        // Three 30-byte gen-0 lines cross the 64-byte limit; the NEXT
        // write rotates gen 0 into `.1` (no `.2` yet — only one shift).
        for ch in ['a', 'b', 'c'] {
            logger.write_line(&line(0, ch));
        }
        logger.write_line(&line(1, 'd')); // rotate: gen0 -> .1
        for ch in ['e', 'f'] {
            logger.write_line(&line(2, ch)); // gen 2 fills up
        }
        logger.write_line(&line(3, 'g')); // rotate: gen0 -> .2, gen2 -> .1
        logger.write_line(&line(4, 'h'));

        let names: Vec<String> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names.len(),
            3,
            "exactly synso.log + two generations, got {names:?}"
        );
        for expected in ["synso.log", "synso.log.1", "synso.log.2"] {
            assert!(names.contains(&expected.to_owned()), "missing {expected}");
        }

        let current = std::fs::read_to_string(tmp.path().join("synso.log")).unwrap();
        assert!(current.starts_with("gen-3"), "current: {current:?}");
        assert!(
            !current.contains("gen-0"),
            "gen-0 leaked into the current log: {current:?}"
        );
        let shifted = std::fs::read_to_string(tmp.path().join("synso.log.1")).unwrap();
        assert!(shifted.contains("gen-2"), "shifted: {shifted:?}");
        let oldest = std::fs::read_to_string(tmp.path().join("synso.log.2")).unwrap();
        assert!(oldest.contains("gen-0-line-a"), "oldest: {oldest:?}");

        // One more generation evicts gen-0 from disk for good.
        for ch in ['i', 'j'] {
            logger.write_line(&line(5, ch));
        }
        logger.write_line(&line(6, 'k')); // rotate: gen0's file removed
        let oldest = std::fs::read_to_string(tmp.path().join("synso.log.2")).unwrap();
        assert!(
            !oldest.contains("gen-0-line-a") && oldest.contains("gen-2"),
            "oldest generation survived its eviction: {oldest:?}"
        );
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 3);
    }
}
