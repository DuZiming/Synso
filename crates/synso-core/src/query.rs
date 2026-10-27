//! Query execution over the hot index: case-insensitive substring search
//! against the name arena with prefix/word-start/substring ranking, a
//! within-class recency boost, deterministic path tie-breaks and
//! limit/offset paging.
//!
//! Match target (v1): the lowercased NAME arena only — path matching (the
//! Task 7 `path:` filter) and pinyin keys (Task 5) arrive later and will
//! widen WHAT is matched, not how results are ranked or paged.
//!
//! Performance shape: one linear pass collects a small
//! `(class, recency, path_len, slot)` key per match — never a path string
//! for the whole matching population (the plan's "no collecting all paths
//! at 1M files" ruling; sorting those keys is the sanctioned simpler
//! alternative to a bounded Top-N heap) — and full paths materialize only
//! for the returned page.

use std::cmp::Ordering;
use std::path::MAIN_SEPARATOR;

use crate::hotindex::HotIndex;
use crate::model::FileId;

/// Within-class recency boost window: matches whose mtime is within the
/// last 30 days (inclusive) of the query time outrank same-class matches
/// with older mtimes.
const RECENCY_WINDOW_SECS: i64 = 30 * 86_400;

/// One fully materialized search-result row. Paths are only built for
/// returned pages, never for the whole matching population.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// The `files` row id (what reveal/trash/collections key on later).
    pub id: FileId,
    /// Full path rebuilt from the interned dir tree — root segment, ...,
    /// parent dirs, name — in the ORIGINAL spelling, joined with
    /// [`MAIN_SEPARATOR`]. This is the index-relative spelling (leading
    /// scan-root segment, no drive prefix).
    pub path: String,
    /// Final path component, original spelling.
    pub name: String,
    pub size: u64,
    /// Unix-epoch seconds.
    pub mtime: i64,
    /// Extension text (lowercase, no leading dot); empty for no extension.
    pub ext: String,
    pub is_dir: bool,
}

/// One page of [`Hit`]s plus the full match count: `hits` covers
/// `[offset, offset + limit)` of the ranked ordering.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchResult {
    pub hits: Vec<Hit>,
    pub total: u64,
}

/// Primary rank signal: where the query matched inside the name.
/// Declaration order IS the ranking order (the derived `Ord` follows it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum MatchClass {
    /// Anywhere inside a word.
    Substring,
    /// At the start of a delimiter-separated word (`.`, `-`, `_`, space...).
    WordStart,
    /// At the very start of the name.
    Prefix,
}

/// Runs `text` against `hot`; [`crate::engine::Engine::search`] is the
/// public wrapper that computes `now` from the system clock (tests inject a
/// fixed `now` so the recency window is deterministic). A cold index
/// returns an empty result — the engine serves the last known-good snapshot
/// while loading, and a cold index holds no data anyway.
pub(crate) fn run(hot: &HotIndex, text: &str, offset: u32, limit: u32, now: i64) -> SearchResult {
    if !hot.is_warm() {
        return SearchResult::default();
    }
    // Empty text lists the most recent entries; anything else is a
    // case-insensitive substring query against the name arena (the query is
    // lowercased to match the arena's spelling; non-alphanumeric queries
    // simply match literally).
    let ranked = if text.is_empty() {
        recent_slots(hot)
    } else {
        match_slots(hot, &text.to_lowercase(), now)
    };
    SearchResult {
        total: ranked.len() as u64,
        hits: page(hot, &ranked, offset, limit),
    }
}

/// Ranking key for one matching slot. Path STRINGS are deliberately absent:
/// at a million matches they would dominate the cost; `path_len` (a cheap
/// tree walk) and — only on full ties — lazily rebuilt segment chains
/// provide the deterministic order. `(class, recent)` ordering is the
/// brief's "+1 within the same class" score, kept as a tuple so the boost
/// can never cross a class boundary.
struct Scored {
    class: MatchClass,
    recent: bool,
    path_len: u64,
    slot: usize,
}

/// The substring pass: classify every name-arena hit, rank, return slots in
/// final order.
fn match_slots(hot: &HotIndex, query: &str, now: i64) -> Vec<usize> {
    let mut keys: Vec<Scored> = Vec::new();
    for slot in 0..hot.slot_count() {
        let Some(class) = classify(hot.name_lower(slot), query) else {
            continue;
        };
        keys.push(Scored {
            class,
            recent: hot.mtime(slot) >= now - RECENCY_WINDOW_SECS,
            path_len: hot.path_len(slot),
            slot,
        });
    }
    keys.sort_by(|a, b| cmp_scored(hot, a, b));
    keys.into_iter().map(|key| key.slot).collect()
}

/// Ranking order: better class, then recency, then shorter full path, then
/// lexicographic full path, then slot. Distinct slots never reconstruct the
/// same path (`dirs` and `files` both have UNIQUE(parent/name) keys), so
/// the final slot comparison is an unreachable total-order guard.
fn cmp_scored(hot: &HotIndex, a: &Scored, b: &Scored) -> Ordering {
    b.class
        .cmp(&a.class)
        .then(b.recent.cmp(&a.recent))
        .then(a.path_len.cmp(&b.path_len))
        .then_with(|| cmp_slot_paths(hot, a.slot, b.slot))
        .then(a.slot.cmp(&b.slot))
}

/// Key for the empty-query "most recent" mode.
struct Recent {
    mtime: i64,
    path_len: u64,
    slot: usize,
}

/// Every slot, newest mtime first (ties: shorter path, then path ascending).
fn recent_slots(hot: &HotIndex) -> Vec<usize> {
    let mut keys: Vec<Recent> = Vec::with_capacity(hot.slot_count());
    for slot in 0..hot.slot_count() {
        keys.push(Recent {
            mtime: hot.mtime(slot),
            path_len: hot.path_len(slot),
            slot,
        });
    }
    keys.sort_by(|a, b| {
        b.mtime
            .cmp(&a.mtime)
            .then(a.path_len.cmp(&b.path_len))
            .then_with(|| cmp_slot_paths(hot, a.slot, b.slot))
            .then(a.slot.cmp(&b.slot))
    });
    keys.into_iter().map(|key| key.slot).collect()
}

/// Materializes the `[offset, offset + limit)` slice of the ranked slots.
/// Hits are the only place full paths are built.
fn page(hot: &HotIndex, ranked: &[usize], offset: u32, limit: u32) -> Vec<Hit> {
    let start = (offset as usize).min(ranked.len());
    let end = start.saturating_add(limit as usize).min(ranked.len());
    ranked[start..end]
        .iter()
        .map(|&slot| hit_of(hot, slot))
        .collect()
}

fn hit_of(hot: &HotIndex, slot: usize) -> Hit {
    Hit {
        id: hot.file_id(slot),
        path: hot.path_of(slot),
        name: hot.name_original(slot).to_string(),
        size: hot.size(slot),
        mtime: hot.mtime(slot),
        ext: hot.ext_of(hot.ext_id(slot)).to_string(),
        is_dir: hot.is_dir_entry(slot),
    }
}

/// Lexicographic ordering of the two slots' full reconstructed paths,
/// compared segment-wise so no path string is built.
fn cmp_slot_paths(hot: &HotIndex, a: usize, b: usize) -> Ordering {
    let mut chain_a = hot.dir_chain(a);
    chain_a.push(hot.name_original(a));
    let mut chain_b = hot.dir_chain(b);
    chain_b.push(hot.name_original(b));
    cmp_path_segments(&chain_a, &chain_b)
}

/// Compares two `MAIN_SEPARATOR`-joined paths given as segment lists,
/// exactly as comparing the joined strings byte-for-byte would: when one
/// segment is a proper prefix of the other, the joined strings next see the
/// separator on the shorter side and the longer side's next byte, and a
/// separator can never appear inside a segment. A path whose segments are a
/// prefix of another's is a proper prefix of the longer joined path.
fn cmp_path_segments(a: &[&str], b: &[&str]) -> Ordering {
    let sep = MAIN_SEPARATOR as u8;
    for (seg_a, seg_b) in a.iter().zip(b.iter()) {
        match cmp_segment_with_sep(seg_a.as_bytes(), seg_b.as_bytes(), sep) {
            Ordering::Equal => continue,
            ord => return ord,
        }
    }
    a.len().cmp(&b.len())
}

/// One segment pair of [`cmp_path_segments`], with the separator that
/// follows the shorter side folded into the comparison.
fn cmp_segment_with_sep(x: &[u8], y: &[u8], sep: u8) -> Ordering {
    let common = x.len().min(y.len());
    match x[..common].cmp(&y[..common]) {
        Ordering::Equal => {}
        ord => return ord,
    }
    match (x.get(common), y.get(common)) {
        // The longer segment's next byte versus the separator that follows
        // the shorter one in the joined path.
        (None, Some(&byte)) => sep.cmp(&byte),
        (Some(&byte), None) => byte.cmp(&sep),
        _ => Ordering::Equal,
    }
}

/// Where `query` (both sides already lowercased) matches inside `name`:
/// [`MatchClass::Prefix`] for an occurrence at index 0,
/// [`MatchClass::WordStart`] when some occurrence starts right after a
/// non-alphanumeric char, [`MatchClass::Substring`] for any other
/// occurrence; `None` when the name does not contain the query at all.
fn classify(name: &str, query: &str) -> Option<MatchClass> {
    let mut best: Option<MatchClass> = None;
    for (idx, _) in name.match_indices(query) {
        let class = if idx == 0 {
            MatchClass::Prefix
        } else {
            // `idx` is a char boundary, so the char ending at it is the one
            // immediately before the match.
            match name[..idx].chars().next_back() {
                Some(prev) if !prev.is_alphanumeric() => MatchClass::WordStart,
                _ => MatchClass::Substring,
            }
        };
        if class == MatchClass::Prefix {
            return Some(MatchClass::Prefix); // cannot do better; stop scanning
        }
        best = Some(best.map_or(class, |best| best.max(class)));
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Engine, EngineEvent, EngineOptions};
    use crate::model::FileRecord;
    use crate::store::Store;
    use crossbeam_channel::Receiver;
    use std::fs;
    use std::path::Path;
    use std::time::Duration;

    /// Fixed query time for ranking tests: they control mtimes exactly, so
    /// the 30-day recency window is deterministic.
    const NOW: i64 = 2_000_000_000;
    const DAY: i64 = 86_400;

    /// Builds a warm hot index from `(dir_path, name, mtime, is_dir)` rows
    /// so ranking tests pin mtimes precisely (the engine path cannot set
    /// mtimes at MSRV 1.83).
    fn indexed(rows: &[(&[&str], &str, i64, bool)]) -> HotIndex {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open(&tmp.path().join("index.db")).unwrap();
        for (dir_path, name, mtime, is_dir) in rows {
            let mut rec = FileRecord {
                id: 0,
                parent: 0,
                name: (*name).to_string(),
                size: 1,
                mtime: *mtime,
                ctime: *mtime,
                atime: *mtime,
                ext_id: 0,
                is_dir: *is_dir,
                hash_first: None,
                hash_full: None,
            };
            store.upsert_file(dir_path, &mut rec).unwrap();
        }
        HotIndex::load(&store).unwrap()
    }

    fn names(res: &SearchResult) -> Vec<&str> {
        res.hits.iter().map(|hit| hit.name.as_str()).collect()
    }

    fn open_engine(db: &Path, root: &Path) -> Engine {
        Engine::open(
            db,
            EngineOptions {
                volumes: vec![root.to_string_lossy().into_owned()],
                exclusions: Vec::new(),
            },
        )
        .unwrap()
    }

    fn wait_scan_finished(rx: &Receiver<EngineEvent>) {
        loop {
            match rx.recv_timeout(Duration::from_secs(60)).unwrap() {
                EngineEvent::ScanProgress { .. } => continue,
                EngineEvent::ScanFinished { .. } => return,
                other => panic!("unexpected event: {other:?}"),
            }
        }
    }

    #[test]
    fn search_substring_case_insensitive() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = tmp.path().join("fixture");
        fs::create_dir_all(root.join("Docs")).unwrap();
        fs::write(root.join("report.docx"), b"r").unwrap();
        fs::write(root.join("Docs").join("annual-REPORT.pdf"), b"a").unwrap();
        fs::write(root.join("readme.md"), b"x").unwrap();

        let engine = open_engine(&db_path, &root);
        let rx = engine.subscribe();
        engine.start_full_scan();
        wait_scan_finished(&rx);

        // The uppercase query matches the lowercase name arena ...
        let res = engine.search("REPORT", 0, 10);
        assert_eq!(res.total, 2, "report.docx and annual-REPORT.pdf match");
        // ... and the prefix hit ranks over the word-start hit.
        assert_eq!(names(&res), vec!["report.docx", "annual-REPORT.pdf"]);

        let hit = &res.hits[0];
        let sep = MAIN_SEPARATOR;
        assert_eq!(
            hit.path,
            format!("fixture{sep}report.docx"),
            "path rebuilt from the interned dir tree, original spelling"
        );
        assert_eq!(hit.name, "report.docx");
        assert_eq!(hit.ext, "docx");
        assert_eq!(hit.size, 1);
        assert!(!hit.is_dir);
        assert!(hit.mtime > 0);
        assert!(hit.id > 0, "the files row id travels into the hit");

        // The second hit keeps its original mixed-case spelling everywhere.
        assert_eq!(
            res.hits[1].path,
            format!("fixture{sep}Docs{sep}annual-REPORT.pdf")
        );
        // A lowercase query behaves identically ...
        assert_eq!(engine.search("report", 0, 10).total, 2);
        // ... and non-matching names stay out of the result.
        assert_eq!(engine.search("readme", 0, 10).total, 1);
    }

    #[test]
    fn search_ranks_prefix_over_substring() {
        let hot = indexed(&[
            // (dir, name, mtime, is_dir); class of the "report" query in comments
            (&["a"], "report.docx", NOW - DAY, false), // prefix, recent
            (&["a"], "report-ancient.docx", NOW - 90 * DAY, false), // prefix, old
            (&["a"], "zz-report.docx", NOW - DAY, false), // word-start, recent
            (&["a"], "myreport.docx", NOW - DAY, false), // substring, recent
        ]);

        let res = run(&hot, "report", 0, 10, NOW);
        assert_eq!(res.total, 4);
        assert_eq!(
            names(&res),
            vec![
                "report.docx",         // prefix + recent beats everything
                "report-ancient.docx", // prefix class survives without the recency boost
                "zz-report.docx",      // word-start
                "myreport.docx",       // plain substring
            ]
        );
    }

    #[test]
    fn search_empty_returns_recent() {
        let hot = indexed(&[
            (&["d"], "old.txt", NOW - 1000 * DAY, false),
            (&["d"], "new.txt", NOW - DAY, false),
            (&["d"], "mid.txt", NOW - 500 * DAY, false),
            (&["d"], "tie.txt", NOW - 500 * DAY, false), // mtime tie with mid.txt
            (&["d"], "somedir", NOW - 60, true),         // directory entries rank too
        ]);

        // Most recent first; the mtime tie (mid/tie) breaks by path:
        // equal length, then lexicographic ("d\mid.txt" < "d\tie.txt").
        let res = run(&hot, "", 0, 10, NOW);
        assert_eq!(res.total, 5);
        assert_eq!(
            names(&res),
            vec!["somedir", "new.txt", "mid.txt", "tie.txt", "old.txt"]
        );

        // limit/offset page the recent ordering the same way.
        let page = run(&hot, "", 2, 2, NOW);
        assert_eq!(page.total, 5);
        assert_eq!(names(&page), vec!["mid.txt", "tie.txt"]);
    }

    #[test]
    fn search_paging_and_total() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = tmp.path().join("many");
        fs::create_dir_all(&root).unwrap();
        for i in 0..12 {
            fs::write(root.join(format!("doc{i:02}.txt")), b"x").unwrap();
        }
        fs::write(root.join("notes.md"), b"n").unwrap();
        fs::write(root.join("zz.txt"), b"z").unwrap();

        let engine = open_engine(&db_path, &root);
        let rx = engine.subscribe();
        engine.start_full_scan();
        wait_scan_finished(&rx);

        // All 12 doc*.txt match, the other two do not. Within the matches
        // the order is deterministic: same class (prefix), same recency
        // (all just written), same path length, lexicographic names.
        let page1 = engine.search("doc", 0, 5);
        assert_eq!(page1.total, 12);
        assert_eq!(
            names(&page1),
            vec![
                "doc00.txt",
                "doc01.txt",
                "doc02.txt",
                "doc03.txt",
                "doc04.txt"
            ]
        );

        let page2 = engine.search("doc", 5, 5);
        assert_eq!(page2.total, 12);
        assert_eq!(
            names(&page2),
            vec![
                "doc05.txt",
                "doc06.txt",
                "doc07.txt",
                "doc08.txt",
                "doc09.txt"
            ]
        );

        // A last partial page ...
        let page3 = engine.search("doc", 10, 5);
        assert_eq!(page3.total, 12);
        assert_eq!(names(&page3), vec!["doc10.txt", "doc11.txt"]);

        // ... an offset past the end, and a zero limit, still report the
        // full match count.
        let past = engine.search("doc", 12, 5);
        assert_eq!((past.hits.len() as u64, past.total), (0, 12));
        let zero = engine.search("doc", 0, 0);
        assert_eq!((zero.hits.len() as u64, zero.total), (0, 12));

        // Empty text lists every entry (12 doc files + 2 others).
        let recent = engine.search("", 0, 14);
        assert_eq!((recent.hits.len() as u64, recent.total), (14, 14));
    }

    #[test]
    fn search_cjk_emoji_query() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = tmp.path().join("cjkroot");
        fs::create_dir_all(root.join("文档")).unwrap();
        fs::write(root.join("文档").join("年度报告.docx"), b"d").unwrap();
        fs::create_dir_all(root.join("🎉party")).unwrap();
        fs::write(root.join("🎉party").join("celebration🎉.txt"), b"e").unwrap();
        fs::write(root.join("plain.txt"), b"p").unwrap();

        let engine = open_engine(&db_path, &root);
        let rx = engine.subscribe();
        engine.start_full_scan();
        wait_scan_finished(&rx);

        // A CJK query matches the name containing the same characters.
        let cjk = engine.search("报告", 0, 10);
        assert_eq!(cjk.total, 1);
        assert_eq!(names(&cjk), vec!["年度报告.docx"]);
        let sep = MAIN_SEPARATOR;
        assert_eq!(
            cjk.hits[0].path,
            format!("cjkroot{sep}文档{sep}年度报告.docx")
        );
        assert_eq!(cjk.hits[0].ext, "docx");

        // An emoji query does not panic and matches every name containing
        // it — the directory entry (prefix hit) before the file (substring).
        let emoji = engine.search("🎉", 0, 10);
        assert_eq!(emoji.total, 2);
        assert_eq!(names(&emoji), vec!["🎉party", "celebration🎉.txt"]);
        assert!(emoji.hits[0].is_dir);
        assert!(!emoji.hits[1].is_dir);

        // A query matching nothing is an empty result, not a panic.
        assert_eq!(engine.search("不存在的名字xyz", 0, 10).total, 0);
    }

    #[test]
    fn search_cold_index_returns_empty() {
        let cold = HotIndex::cold();
        assert_eq!(run(&cold, "anything", 0, 10, NOW), SearchResult::default());
        assert_eq!(run(&cold, "", 0, 10, NOW), SearchResult::default());
    }

    #[test]
    fn classify_prefix_word_start_substring() {
        use MatchClass::*;
        // At index 0 the match is a prefix ...
        assert_eq!(classify("report.docx", "report"), Some(Prefix));
        // ... right after a delimiter it is a word start ...
        assert_eq!(classify("annual-report.pdf", "report"), Some(WordStart));
        assert_eq!(classify("my_report.docx", "report"), Some(WordStart));
        assert_eq!(classify("docs report.pdf", "report"), Some(WordStart));
        // ... inside a word it is a plain substring.
        assert_eq!(classify("myreport.docx", "report"), Some(Substring));
        // CJK chars are alphanumeric: the match inside 年度报告 is a substring.
        assert_eq!(classify("年度报告.docx", "报告"), Some(Substring));
        // The best occurrence wins; none of these is a prefix.
        assert_eq!(classify("x-report report", "report"), Some(WordStart));
        // No match at all.
        assert_eq!(classify("report", "reports"), None);
        assert_eq!(classify("readme.md", "zzz"), None);
    }
}
