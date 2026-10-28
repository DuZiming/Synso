//! Query execution over the hot index: case-insensitive substring search
//! against the name arena with prefix/word-start/substring ranking, a
//! within-class recency boost, deterministic path tie-breaks and
//! limit/offset paging.
//!
//! Match target (v1): the lowercased NAME arena plus the two pinyin key
//! arenas (`py_full` / `py_init`) — a query hits when it substring-matches
//! any of the three. Literal name matches keep their prefix/word-start/
//! substring class; pinyin-only matches always classify as Substring, so
//! they never outrank a literal match of the same file. Multiple free-text
//! words must ALL match (AND); the slot's class is its best word's class.
//!
//! Modifiers (Task 7): [`parse_query`] splits the raw string into
//! whitespace tokens; `ext:` / `size:` / `path:` / `coll:` tokens become
//! dimension filters on [`crate::model::ParsedQuery`], a bad value
//! (`size:abc`) falls back verbatim into the free text. Filters run as
//! fast predicates BEFORE ranking and classification (the private
//! `passes_filters` chain); ranking and paging are unchanged by filtering.
//!
//! Performance shape: one linear pass collects a small
//! `(class, recency, path_len, slot)` key per match — never a path string
//! for the whole matching population (the plan's "no collecting all paths
//! at 1M files" ruling; sorting those keys is the sanctioned simpler
//! alternative to a bounded Top-N heap) — and full paths materialize only
//! for the returned page (plus, when a `path:` filter is active, one
//! materialization per filter survivor — see `passes_filters`).

use std::cmp::Ordering;
use std::collections::HashSet;
use std::path::MAIN_SEPARATOR;

use crate::hotindex::HotIndex;
use crate::model::{FileId, ParsedQuery};

/// Within-class recency boost window: matches whose mtime is within the
/// last 30 days (inclusive) of the query time outrank same-class matches
/// with older mtimes.
const RECENCY_WINDOW_SECS: i64 = 30 * 86_400;

/// One fully materialized search-result row. Paths are only built for
/// returned pages, never for the whole matching population.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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

/// Parses a raw query string into free text plus dimension filters (the
/// Everything-style `ext:` / `size:` / `path:` / `coll:` modifiers — see
/// [`ParsedQuery`]). Pure: no I/O, no engine. A modifier token with a BAD
/// value (`size:abc`, a lone `ext:`) is not an error — the whole token
/// falls back verbatim into the free text, so the query degrades to a
/// literal match. Never panics, on any input.
pub fn parse_query(s: &str) -> ParsedQuery {
    let mut parsed = ParsedQuery::default();
    let mut free: Vec<&str> = Vec::new();
    for token in s.split_whitespace() {
        match parse_modifier(token) {
            // Repeated ext:/path: tokens win with their last occurrence.
            Some(Modifier::Ext(ext)) => parsed.ext = Some(ext),
            Some(Modifier::Path(path)) => parsed.path_prefix = Some(path),
            Some(Modifier::Coll(name)) => parsed.collections.push(name),
            // Repeated size: tokens combine into the tightest bounds.
            Some(Modifier::Size(min, max)) => {
                parsed.min_size = tighten_bound(parsed.min_size, min, u64::max);
                parsed.max_size = tighten_bound(parsed.max_size, max, u64::min);
            }
            // Not a modifier (or a bad value): the token stays free text
            // verbatim, so the query degrades to a literal match.
            None => free.push(token),
        }
    }
    parsed.text = free.join(" ");
    parsed
}

/// One recognized modifier token (the `Modifier::Size` payload is the
/// token's `(min, max)` effect; each side `None` when the token does not
/// constrain that side).
enum Modifier {
    Ext(String),
    Size(Option<u64>, Option<u64>),
    Path(String),
    Coll(String),
}

/// Parses one whitespace-free token into a modifier. `None` when the token
/// is no `kw:value` modifier at all, or when the keyword is one but the
/// value is bad — either way the caller keeps the token as free text.
fn parse_modifier(token: &str) -> Option<Modifier> {
    let (keyword, value) = token.split_once(':')?;
    match keyword.to_ascii_lowercase().as_str() {
        "ext" => valid_ext(value).map(Modifier::Ext),
        "size" => parse_size(value).map(|(min, max)| Modifier::Size(min, max)),
        // Any non-empty string is a usable path substring / collection
        // name; the value is kept as typed (matching lowercases / the
        // store compares exactly).
        "path" => (!value.is_empty()).then(|| Modifier::Path(value.to_string())),
        "coll" => (!value.is_empty()).then(|| Modifier::Coll(value.to_string())),
        _ => None,
    }
}

/// A usable `ext:` value: non-empty and free of the characters an interned
/// extension cannot contain — dots (std splits extensions on the LAST dot,
/// so `Path::extension` never yields one), path separators, and the
/// modifier colon. Returned lowercased, matching how the store interns
/// extension text.
fn valid_ext(value: &str) -> Option<String> {
    (!value.is_empty() && !value.contains(['.', ':', '/', '\\']))
        .then(|| value.to_ascii_lowercase())
}

/// Unit multipliers: binary sizes (1 kb = 1024 b), the convention file
/// managers — and the fixture sizes tests write — use.
const UNIT_KB: u64 = 1024;
const UNIT_MB: u64 = UNIT_KB * UNIT_KB;
const UNIT_GB: u64 = UNIT_MB * UNIT_KB;

/// The byte multiplier of a size suffix; `""` (a bare number) is bytes.
fn unit_multiplier(suffix: &str) -> Option<u64> {
    match suffix.to_ascii_lowercase().as_str() {
        "" | "b" => Some(1),
        "kb" => Some(UNIT_KB),
        "mb" => Some(UNIT_MB),
        "gb" => Some(UNIT_GB),
        _ => None,
    }
}

/// Parses a `size:` value into its `(min, max)` effect on the inclusive
/// byte bounds (each side `None` when unconstrained): the operator forms
/// `>` `>=` `<` `<=` `=` (strict operators shift by one into inclusive
/// bounds), the range form `<n1>-<n2><unit?>`, and the bare form (exact).
/// `None` for any other shape — a bad value falls back to free text.
fn parse_size(value: &str) -> Option<(Option<u64>, Option<u64>)> {
    let (op, rest) = if let Some(rest) = value.strip_prefix(">=") {
        (">=", rest)
    } else if let Some(rest) = value.strip_prefix("<=") {
        ("<=", rest)
    } else if let Some(rest) = value.strip_prefix('>') {
        (">", rest)
    } else if let Some(rest) = value.strip_prefix('<') {
        ("<", rest)
    } else if let Some(rest) = value.strip_prefix('=') {
        ("=", rest)
    } else {
        ("", value)
    };
    if !op.is_empty() {
        let (digits, suffix) = split_digits(rest)?;
        let bytes = bytes_of(digits, suffix)?;
        return Some(match op {
            ">" => (Some(bytes.saturating_add(1)), None),
            ">=" => (Some(bytes), None),
            "<" => (None, Some(bytes.saturating_sub(1))),
            "<=" => (None, Some(bytes)),
            _ => (Some(bytes), Some(bytes)), // "="
        });
    }
    // Range form: bare first number, '-', second number with the shared
    // optional trailing unit (`size:1mb-5mb` is NOT the grammar).
    if let Some((left, right)) = value.split_once('-') {
        let (digits, no_unit) = split_digits(left)?;
        if !no_unit.is_empty() {
            return None;
        }
        let (digits_hi, suffix) = split_digits(right)?;
        let multiplier = unit_multiplier(suffix)?;
        let low = digits.parse::<u64>().ok()?.checked_mul(multiplier)?;
        let high = digits_hi.parse::<u64>().ok()?.checked_mul(multiplier)?;
        return Some((Some(low), Some(high))); // inverted ranges simply match nothing
    }
    // Bare form: an exact size in bytes.
    let (digits, suffix) = split_digits(value)?;
    let bytes = bytes_of(digits, suffix)?;
    Some((Some(bytes), Some(bytes)))
}

/// `Some((leading ascii digits, the rest))` of `rest`; `None` when it does
/// not start with at least one ascii digit.
fn split_digits(rest: &str) -> Option<(&str, &str)> {
    let split = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    (split > 0).then(|| (&rest[..split], &rest[split..]))
}

/// `digits * multiplier`, as bytes; `None` on an unknown unit or a number
/// (or product) that does not fit in u64 — an unrepresentable size is a
/// bad value, not a saturated one.
fn bytes_of(digits: &str, suffix: &str) -> Option<u64> {
    digits
        .parse::<u64>()
        .ok()?
        .checked_mul(unit_multiplier(suffix)?)
}

/// Folds one modifier's effect into a stored bound, keeping the combined
/// bound tightest: `pick` is `u64::max` for lower bounds and `u64::min`
/// for upper ones, so a later token can only ever narrow the range.
fn tighten_bound(
    stored: Option<u64>,
    effect: Option<u64>,
    pick: fn(u64, u64) -> u64,
) -> Option<u64> {
    match (stored, effect) {
        (Some(a), Some(b)) => Some(pick(a, b)),
        (bound, None) => bound,
        (None, bound) => bound,
    }
}

/// Runs `text` against `hot`; [`crate::engine::Engine::search`] is the
/// public wrapper that computes `now` from the system clock (tests inject a
/// fixed `now` so the recency window is deterministic). A cold index
/// returns an empty result — the engine serves the last known-good snapshot
/// while loading, and a cold index holds no data anyway.
#[cfg(test)]
pub(crate) fn run(hot: &HotIndex, text: &str, offset: u32, limit: u32, now: i64) -> SearchResult {
    run_parsed(hot, &parse_query(text), None, offset, limit, now)
}

/// [`run`], over an already-parsed query plus the resolved `coll:` filter
/// (`None` = no collection constraint; the engine resolves names to this
/// membership set). [`crate::engine::Engine::search_parsed`] is the public
/// wrapper. The parsed dimension filters run as fast predicates BEFORE
/// ranking and classification; ranking itself is unchanged.
pub(crate) fn run_parsed(
    hot: &HotIndex,
    q: &ParsedQuery,
    coll_members: Option<&HashSet<FileId>>,
    offset: u32,
    limit: u32,
    now: i64,
) -> SearchResult {
    if !hot.is_warm() {
        return SearchResult::default();
    }
    // Resolve the ext filter to an interned id ONCE: the per-slot check is
    // then an integer compare. An extension nothing carries (never
    // interned) means the query cannot match anything.
    let ext_id = match &q.ext {
        Some(want) => match hot.ext_id_of(want) {
            Some(id) => Some(id),
            None => return SearchResult::default(),
        },
        None => None,
    };
    // The path needle lowercases once; slot paths lowercase per check.
    let path_needle: Option<String> = q.path_prefix.as_deref().map(str::to_lowercase);
    // Empty text lists the most recent entries; anything else requires
    // EVERY free-text word to hit the name or a pinyin key (AND).
    let words: Vec<String> = q.text.split_whitespace().map(str::to_lowercase).collect();
    let (total, ranked) = if words.is_empty() {
        recent_slots(
            hot,
            q,
            ext_id,
            coll_members,
            path_needle.as_deref(),
            offset,
            limit,
        )
    } else {
        match_slots(
            hot,
            q,
            ext_id,
            coll_members,
            path_needle.as_deref(),
            &words,
            now,
            offset,
            limit,
        )
    };
    SearchResult {
        total,
        hits: page(hot, &ranked, offset, limit),
    }
}


/// The fast-reject chain both ranking modes run per slot BEFORE any
/// ranking or classification: ext id and size are integer compares,
/// collection membership one hash lookup. The `path:` filter is the one
/// expensive predicate, so it runs LAST and only for slots that survived
/// the cheap ones — path-filter strategy (ruling 3): MATERIALIZE-AND-CHECK
/// per surviving slot (`path_of` + one lowercase + `contains`), chosen over
/// a segment-wise dir-chain walk because a substring may span separator
/// boundaries (`path:cs\proj`, `path:cs/proj`), which segment-wise checks
/// handle awkwardly; full paths still never materialize for the whole
/// population, only for post-filter candidates.
fn passes_filters(
    hot: &HotIndex,
    q: &ParsedQuery,
    ext_id: Option<i64>,
    coll_members: Option<&HashSet<FileId>>,
    path_needle: Option<&str>,
    slot: usize,
) -> bool {
    if let Some(want) = ext_id {
        if hot.ext_id(slot) != want {
            return false;
        }
    }
    if let Some(min) = q.min_size {
        if hot.size(slot) < min {
            return false;
        }
    }
    if let Some(max) = q.max_size {
        if hot.size(slot) > max {
            return false;
        }
    }
    if let Some(members) = coll_members {
        if !members.contains(&hot.file_id(slot)) {
            return false;
        }
    }
    if let Some(needle) = path_needle {
        if !hot.path_of(slot).to_lowercase().contains(needle) {
            return false;
        }
    }
    true
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

/// The substring pass: filter first, then classify every name/pinyin hit
/// of ALL free-text words, keeping only the best `page_len(offset, limit)`
/// matches in final rank order (see [`insert_top_k`]). Returns `(total,
/// ranked_prefix)` — `total` counts every match (the full result size the
/// UI reports), `ranked_prefix` holds the `[0, offset + limit)` head of
/// the full ranking, which is all [`page`] can ever materialize.
fn match_slots(
    hot: &HotIndex,
    q: &ParsedQuery,
    ext_id: Option<i64>,
    coll_members: Option<&HashSet<FileId>>,
    path_needle: Option<&str>,
    words: &[String],
    now: i64,
    offset: u32,
    limit: u32,
) -> (u64, Vec<usize>) {
    let page_len = page_len(offset, limit);
    let mut total = 0u64;
    let mut top: Vec<Scored> = Vec::new();
    for slot in 0..hot.slot_count() {
        if !passes_filters(hot, q, ext_id, coll_members, path_needle, slot) {
            continue;
        }
        let Some(class) = classify_words(hot, slot, words) else {
            continue;
        };
        total += 1;
        insert_top_k(&mut top, page_len, Scored {
            class,
            recent: hot.mtime(slot) >= now - RECENCY_WINDOW_SECS,
            path_len: hot.path_len(slot),
            slot,
        }, |a, b| cmp_scored(hot, a, b));
    }
    (total, top.into_iter().map(|key| key.slot).collect())
}

/// How many ranked slots [`page`] can ever touch: the run returns the
/// `[offset, offset + limit)` slice, so selection beyond that head is
/// discarded work at 1M-match scale.
fn page_len(offset: u32, limit: u32) -> usize {
    if limit == 0 {
        0
    } else {
        offset as usize + limit as usize
    }
}

/// Inserts `key` into the best-first `top` buffer, keeping at most `k`
/// entries — the equivalent of a full stable sort of ALL candidates
/// restricted to the top `k`: a candidate is dropped only when the buffer
/// is full and ranks `cmp`-worse-or-equal than the current worst (equals
/// keep the earlier-inserted element, as a stable sort would), and
/// insertion positions after `partition_point` preserve the sorted order.
/// With `k` at page size and `n` candidates this is O(n) comparisons for
/// the common reject path instead of O(n log n) sort comparisons plus the
/// path-tie string work a 1M-element sort would pay.
fn insert_top_k<T>(
    top: &mut Vec<T>,
    k: usize,
    key: T,
    mut cmp: impl FnMut(&T, &T) -> Ordering,
) {
    if k == 0 {
        return;
    }
    if top.len() == k {
        if cmp(&key, top.last().expect("len checked == k > 0")) != Ordering::Less {
            return;
        }
        top.pop();
    }
    let pos = top.partition_point(|existing| cmp(existing, &key) == Ordering::Less);
    top.insert(pos, key);
}


/// Where the free-text words hit `slot`: every word must match (the name
/// arena first, a pinyin key as fallback — the AND semantics); the slot's
/// class is the BEST class across the words.
fn classify_words(hot: &HotIndex, slot: usize, words: &[String]) -> Option<MatchClass> {
    let mut best: Option<MatchClass> = None;
    for word in words {
        let class =
            classify(hot.name_lower(slot), word).or_else(|| pinyin_class(hot, slot, word))?;
        best = Some(best.map_or(class, |best| best.max(class)));
    }
    best
}

/// Ranking order: better class, then recency, then shorter full path, then
/// lexicographic full path, then slot. That final slot comparison is a
/// REACHABLE tie-break, not dead code: under multi-root scans whose roots
/// share a final segment (e.g. `C:\a` and `D:\a`) the colliding root
/// segments land in one `dirs` branch, and SQLite's UNIQUE(parent,name)
/// does not treat two NULL-parent rows as duplicates — so distinct slots
/// can reconstruct colliding full paths. The slot compare keeps the
/// ordering total in that state (the same guard as in `recent_slots`).
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

/// Every filter-passing slot, newest mtime first (ties: shorter path,
/// then path ascending) — the empty-query "most recent" mode. Selection
/// is bounded to the page head, exactly like [`match_slots`].
fn recent_slots(
    hot: &HotIndex,
    q: &ParsedQuery,
    ext_id: Option<i64>,
    coll_members: Option<&HashSet<FileId>>,
    path_needle: Option<&str>,
    offset: u32,
    limit: u32,
) -> (u64, Vec<usize>) {
    let page_len = page_len(offset, limit);
    let mut total = 0u64;
    let mut top: Vec<Recent> = Vec::new();
    for slot in 0..hot.slot_count() {
        if !passes_filters(hot, q, ext_id, coll_members, path_needle, slot) {
            continue;
        }
        total += 1;
        insert_top_k(&mut top, page_len, Recent {
            mtime: hot.mtime(slot),
            path_len: hot.path_len(slot),
            slot,
        }, |a, b| {
            b.mtime
                .cmp(&a.mtime)
                .then(a.path_len.cmp(&b.path_len))
                .then_with(|| cmp_slot_paths(hot, a.slot, b.slot))
                .then(a.slot.cmp(&b.slot))
        });
    }
    (total, top.into_iter().map(|key| key.slot).collect())
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

/// Materializes one fully populated [`Hit`] for `slot` (the only place a
/// full path string is built). `pub(crate)`: the dedup groups view
/// (Task 9) materializes its stored groups' members the same way.
pub(crate) fn hit_of(hot: &HotIndex, slot: usize) -> Hit {
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

/// Pinyin fallback (both sides already lowercased; the query is non-empty
/// here): a query that misses the literal name but substring-matches
/// either pinyin key is a [`MatchClass::Substring`] hit — position inside
/// the key does not matter, so pinyin never outranks a literal match of
/// the same file (or any other file's literal Prefix/WordStart hit).
fn pinyin_class(hot: &HotIndex, slot: usize, query: &str) -> Option<MatchClass> {
    if hot.py_full(slot).contains(query) || hot.py_init(slot).contains(query) {
        Some(MatchClass::Substring)
    } else {
        None
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
                py_full: String::new(),
                py_init: String::new(),
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
    fn search_pinyin_full_and_initials() {
        let hot = indexed(&[
            (&["docs"], "报告.docx", NOW - DAY, false),
            // A literal "bg" name match; must outrank the pinyin-only hit
            // even though it is the older file.
            (&["docs"], "bg.txt", NOW - 90 * DAY, false),
        ]);

        // The full-pinyin query hits via py_full ("baogao.docx").
        let res = run(&hot, "baogao", 0, 10, NOW);
        assert_eq!(res.total, 1);
        assert_eq!(names(&res), vec!["报告.docx"]);

        // The initials query hits via py_init ("bg.docx") — and the literal
        // name match keeps its Prefix class, ranking above the pinyin-only
        // Substring match despite being older.
        let res = run(&hot, "bg", 0, 10, NOW);
        assert_eq!(res.total, 2);
        assert_eq!(names(&res), vec!["bg.txt", "报告.docx"]);
    }

    #[test]
    fn search_pinyin_prefix() {
        let hot = indexed(&[(&["docs"], "年度报告.docx", NOW - DAY, false)]);

        // A pinyin-prefix query ("baog" is the start of "baogao...") is a
        // substring hit on py_full ("niandubaogao.docx").
        let res = run(&hot, "baog", 0, 10, NOW);
        assert_eq!(res.total, 1);
        assert_eq!(names(&res), vec!["年度报告.docx"]);

        // A query that matches neither the name nor either key stays out.
        assert_eq!(run(&hot, "gaoz", 0, 10, NOW).total, 0);
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

    // ------------------- Task 7: parse_query + filters ---------------------

    /// `(min, max)` bounds a one-token size query parses to.
    fn size_bounds(token: &str) -> (Option<u64>, Option<u64>) {
        let q = parse_query(token);
        (q.min_size, q.max_size)
    }

    /// The files row id of the unique hit named `name`.
    fn hit_id(engine: &Engine, name: &str) -> FileId {
        let res = engine.search(name, 0, 50);
        assert_eq!(res.total, 1, "exactly one hit named {name}");
        res.hits[0].id
    }

    #[test]
    fn parse_modifiers_basic() {
        let q = parse_query("报告 ext:docx size:>1mb path:docs coll:工作");
        assert_eq!(q.text, "报告", "only the free-text token remains");
        assert_eq!(q.ext.as_deref(), Some("docx"));
        // `>` is strict: the stored INCLUSIVE lower bound is 1 MiB + 1.
        assert_eq!(q.min_size, Some(1024 * 1024 + 1));
        assert_eq!(q.max_size, None);
        assert_eq!(q.path_prefix.as_deref(), Some("docs"));
        assert_eq!(q.collections, vec!["工作".to_string()]);
    }

    #[test]
    fn parse_size_operator_unit_and_range_forms() {
        let kb = 1024u64;
        let mb = kb * kb;
        let gb = mb * kb;
        // Every operator, both bound sides.
        assert_eq!(size_bounds("size:>=1mb"), (Some(mb), None));
        assert_eq!(size_bounds("size:>1mb"), (Some(mb + 1), None));
        assert_eq!(size_bounds("size:<=10kb"), (None, Some(10 * kb)));
        assert_eq!(size_bounds("size:<10kb"), (None, Some(10 * kb - 1)));
        assert_eq!(size_bounds("size:>0b"), (Some(1), None));
        // `=` and the bare form are exact: both bounds, same value.
        assert_eq!(size_bounds("size:=2mb"), (Some(2 * mb), Some(2 * mb)));
        assert_eq!(size_bounds("size:2048"), (Some(2048), Some(2048)));
        // Units are case-insensitive; a bare number means bytes.
        assert_eq!(size_bounds("size:5KB"), (Some(5 * kb), Some(5 * kb)));
        // The range form applies its one trailing unit to both endpoints.
        assert_eq!(
            size_bounds("size:100-500kb"),
            (Some(100 * kb), Some(500 * kb))
        );
        assert_eq!(size_bounds("size:1-2gb"), (Some(gb), Some(2 * gb)));
        assert_eq!(size_bounds("size:3-4"), (Some(3), Some(4)));
        // An inverted range parses (tightest bounds simply match nothing).
        assert_eq!(size_bounds("size:2-1mb"), (Some(2 * mb), Some(mb)));
    }

    #[test]
    fn parse_repeated_modifiers_last_wins_and_bounds_tighten() {
        // ext / path: the LAST occurrence wins.
        let q = parse_query("ext:pdf ext:docx path:a path:b");
        assert_eq!(q.ext.as_deref(), Some("docx"));
        assert_eq!(q.path_prefix.as_deref(), Some("b"));
        // coll: accumulates (repeats included; execution intersects anyway).
        let q = parse_query("coll:a coll:b coll:a");
        assert_eq!(
            q.collections,
            vec!["a".to_string(), "b".to_string(), "a".to_string()]
        );
        // size: every token tightens the combined bounds.
        let kb = 1024u64;
        let gb = kb * kb * kb;
        let q = parse_query("size:>1kb size:<1gb size:>1mb");
        assert_eq!(q.min_size, Some(kb * kb + 1), "the larger lower bound wins");
        assert_eq!(q.max_size, Some(gb - 1), "the smaller upper bound wins");
    }

    #[test]
    fn parse_query_bad_modifiers_fall_back_to_literal() {
        // A non-numeric size: the WHOLE token stays free text verbatim ...
        let q = parse_query("report size:abc");
        assert_eq!(q.text, "report size:abc");
        assert_eq!((q.min_size, q.max_size), (None, None));
        // ... and so does every other bad value: lone empty modifiers, bad
        // units, fractions, per-side range units, overflowing numbers, and
        // ext values carrying characters an interned extension cannot have.
        for bad in [
            "ext:",
            "size:",
            "path:",
            "coll:",
            "size:>x",
            "size:1zb",
            "size:1.5mb",
            "size:1mb-5mb",
            "size:99999999999999999999999gb",
            "size:-5",
            "size:5-",
            "ext:.pdf",
            "ext:p/df",
            "ext:p\\df",
            "ext:a:b",
        ] {
            let q = parse_query(bad);
            assert_eq!(q.text, bad, "{bad:?} must fall back verbatim");
            assert_eq!(q.ext, None, "{bad:?} must not parse as ext");
            assert_eq!(q.min_size, None, "{bad:?} must not parse as size");
        }
        // Good modifiers leave NO residue in the free text.
        let q = parse_query("ext:pdf");
        assert_eq!(q.text, "");
        // Plain words — including colon-bearing non-modifiers — stay free
        // text, and multiple tokens join with single spaces.
        let q = parse_query("hello  world  http://x");
        assert_eq!(q.text, "hello world http://x");
    }

    #[test]
    fn search_filter_ext_size_path() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = tmp.path().join("fixture");
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::create_dir_all(root.join("other")).unwrap();
        fs::write(
            root.join("docs").join("annual-report-big.docx"),
            vec![0u8; 2 * 1024 * 1024],
        )
        .unwrap();
        fs::write(
            root.join("docs").join("annual-report-small.docx"),
            vec![0u8; 1024 * 1024],
        )
        .unwrap();
        fs::write(root.join("other").join("annual-report.pdf"), b"x").unwrap();
        fs::write(root.join("other").join("tiny.txt"), b"t").unwrap();

        let engine = open_engine(&db_path, &root);
        let rx = engine.subscribe();
        engine.start_full_scan();
        wait_scan_finished(&rx);

        // Baseline: the three "annual" files (the directories do not match).
        assert_eq!(engine.search("annual", 0, 10).total, 3);
        // ext: narrows to the two docx reports ...
        assert_eq!(engine.search("annual ext:docx", 0, 10).total, 2);
        // ... size:>1mb keeps only the strictly bigger one ...
        let big = engine.search("annual ext:docx size:>1mb", 0, 10);
        assert_eq!(
            (big.total, names(&big)),
            (1, vec!["annual-report-big.docx"])
        );
        // ... an exact bound flips to the small one (exactly 1 MiB).
        let small = engine.search("annual ext:docx size:1mb", 0, 10);
        assert_eq!(
            (small.total, names(&small)),
            (1, vec!["annual-report-small.docx"])
        );
        // path: a case-insensitive substring on the full path.
        assert_eq!(engine.search("annual path:docs", 0, 10).total, 2);
        assert_eq!(engine.search("annual path:DOCS", 0, 10).total, 2);
        assert_eq!(engine.search("annual path:other ext:pdf", 0, 10).total, 1);
        // The full combination: text AND ext AND size AND path.
        let combined = engine.search("annual ext:docx size:>1mb path:docs", 0, 10);
        assert_eq!(
            (combined.total, names(&combined)),
            (1, vec!["annual-report-big.docx"])
        );
        // Keywords are case-insensitive; the ext value lowercases.
        assert_eq!(
            engine
                .search("annual EXT:DOCX SIZE:>1MB PATH:DOCS", 0, 10)
                .total,
            1
        );
        // An extension nothing carries filters to empty, not an error.
        assert_eq!(engine.search("annual ext:zzz", 0, 10).total, 0);
        // Filters alone (empty free text) constrain the recent listing.
        assert_eq!(engine.search("ext:txt", 0, 10).total, 1);
    }

    #[test]
    fn search_filter_collection() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = tmp.path().join("fixture");
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::create_dir_all(root.join("other")).unwrap();
        fs::write(root.join("docs").join("alpha.docx"), b"a").unwrap();
        fs::write(root.join("docs").join("beta.docx"), b"b").unwrap();
        fs::write(root.join("other").join("gamma.pdf"), b"g").unwrap();

        let engine = open_engine(&db_path, &root);
        let rx = engine.subscribe();
        engine.start_full_scan();
        wait_scan_finished(&rx);

        let alpha = hit_id(&engine, "alpha.docx");
        let beta = hit_id(&engine, "beta.docx");
        let gamma = hit_id(&engine, "gamma.pdf");
        let work = engine.coll_create("工作").unwrap();
        let backup = engine.coll_create("backup").unwrap();
        engine.coll_create("void").unwrap(); // stays empty
        engine.coll_add(work.id, &[alpha, gamma]).unwrap();
        engine.coll_add(backup.id, &[alpha, beta]).unwrap();

        // coll: alone (empty free text) lists the collection's alive members.
        let res = engine.search("coll:工作", 0, 10);
        let mut listed: Vec<&str> = names(&res);
        listed.sort_unstable();
        assert_eq!((res.total, listed), (2, vec!["alpha.docx", "gamma.pdf"]));

        // Repeated coll: names intersect membership.
        assert_eq!(engine.search("coll:工作 coll:backup", 0, 10).total, 1);
        // The collection filter combines with ext: and free text.
        assert_eq!(engine.search("ext:docx coll:backup", 0, 10).total, 2);
        assert_eq!(engine.search("beta coll:backup", 0, 10).total, 1);
        assert_eq!(engine.search("gamma coll:backup", 0, 10).total, 0);
        // An empty collection intersects anything to nothing.
        assert_eq!(engine.search("coll:工作 coll:void", 0, 10).total, 0);
        // An unknown name filters to no results, not an error.
        let unknown = engine.search("coll:不存在的", 0, 10);
        assert_eq!((unknown.total, unknown.hits.len()), (0, 0));
        // A lone coll: falls back to literal text and matches no name here.
        assert_eq!(engine.search("coll:", 0, 10).total, 0);

        // Membership must be ALIVE: removing gamma drops it from coll:工作.
        engine.coll_remove(work.id, gamma).unwrap();
        assert_eq!(engine.search("coll:工作", 0, 10).total, 1);
    }

    #[test]
    fn search_empty_and_oversized_query() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), b"a").unwrap();
        fs::write(root.join("b.txt"), b"b").unwrap();
        fs::write(root.join("c.txt"), b"c").unwrap();

        let engine = open_engine(&db_path, &root);
        let rx = engine.subscribe();
        engine.start_full_scan();
        wait_scan_finished(&rx);

        // Empty and whitespace-only queries list everything (recent mode).
        assert_eq!(engine.search("", 0, 10).total, 3);
        assert_eq!(engine.search("   ", 0, 10).total, 3);
        // A 300-character query returns normally (no panic), matching nothing.
        let oversized = "z".repeat(300);
        let res = engine.search(&oversized, 0, 10);
        assert_eq!((res.total, res.hits.len()), (0, 0));
        // ... also with a modifier attached, and via the parsed form directly.
        let q = parse_query(&format!("{} ext:txt", "x".repeat(291)));
        assert_eq!(q.text.len(), 291);
        assert_eq!(q.ext.as_deref(), Some("txt"));
        assert_eq!(engine.search_parsed(&q, 0, 10).total, 0);
        // Deeply repeated modifiers never panic either; the unknown coll
        // names filter the result to empty through the resolution path.
        let spam = vec!["ext:pdf ext:docx path:a path:b size:>1b size:<1gb coll:x coll:y zzz"; 20]
            .join(" ");
        let res = engine.search(&spam, 0, 10);
        assert_eq!(res.total, 0);
        // parse_query of empty / whitespace input is the empty query.
        assert_eq!(parse_query(""), ParsedQuery::default());
        assert_eq!(parse_query("  \t "), ParsedQuery::default());
        // An offset past the end still reports the full filtered total.
        let past = engine.search("ext:txt", 5, 5);
        assert_eq!((past.hits.len() as u64, past.total), (0, 3));
    }

    #[test]
    fn search_multi_word_text_is_and() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let root = tmp.path().join("fixture");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("annual-report.pdf"), b"a").unwrap();
        fs::write(root.join("annual-budget.pdf"), b"b").unwrap();
        fs::write(root.join("final-report.pdf"), b"c").unwrap();

        let engine = open_engine(&db_path, &root);
        let rx = engine.subscribe();
        engine.start_full_scan();
        wait_scan_finished(&rx);

        // Multiple free-text words must ALL match, each independently.
        let res = engine.search("annual report", 0, 10);
        assert_eq!((res.total, names(&res)), (1, vec!["annual-report.pdf"]));
        assert_eq!(engine.search("annual", 0, 10).total, 2);
        assert_eq!(engine.search("report pdf", 0, 10).total, 2);
        // A word matching nothing rejects the whole query.
        assert_eq!(engine.search("annual missingword", 0, 10).total, 0);
    }
}
