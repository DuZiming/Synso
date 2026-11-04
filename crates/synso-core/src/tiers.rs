//! The three-tier dimension cache (v1.2): hot (resident, budget-bounded
//! LFU with exponential decay), staging (TTL'd, promoted from cold by a
//! hit-burst rule), cold (not held — the caller reads storage and hands
//! the value to [`TierCache::put_staging`]).
//!
//! Scope: this module knows NOTHING about SQLite or dimensions — it is
//! the generic admission/eviction machinery. The engine wires it to the
//! read-only store connection and the dimension registry
//! (`crate::dimensions`), and the policy values come from
//! [`crate::settings::Settings`] (live-appliable).
//!
//! Policy (see the v1.2 design doc):
//! - recorded hits update Heat per entry (only tier residents hold a
//!   Heat row — the Heat map stays bounded by the tier budgets, never
//!   the access stream);
//! - a key hit `promote_hits` times inside `promote_window` enters
//!   staging (TTL `staging_ttl`, refreshed on each hit);
//! - a key whose decayed hit-count reaches `hot_threshold` enters the hot
//!   tier, which never exceeds `hot_budget` — overflow evicts the
//!   lowest-heat entry;
//! - heat halves every `decay_half_life`; time is injected so tests are
//!   deterministic.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Tunable policy knobs (v1.2 design defaults; every field is exposed in
/// the settings UI and live-appliable — see [`TierCache::set_policy`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryPolicy {
    /// Hard ceiling for the hot tier's VALUES (bookkeeping excluded).
    pub hot_budget_bytes: u64,
    /// Hard ceiling for staging.
    pub staging_budget_bytes: u64,
    /// How long a staging entry survives without a hit.
    pub staging_ttl: Duration,
    /// Hits inside `promote_window` that promote cold → staging.
    pub promote_hits: u32,
    /// The sliding window for the promotion rule.
    pub promote_window: Duration,
    /// Decayed hit-count needed for staging → hot.
    pub hot_threshold: u32,
    /// Heat half-life.
    pub decay_half_life: Duration,
    /// Preload the hot tier from persisted heat at startup.
    pub preload_on_start: bool,
}

impl Default for MemoryPolicy {
    fn default() -> Self {
        MemoryPolicy {
            hot_budget_bytes: 64 * 1024 * 1024,
            staging_budget_bytes: 32 * 1024 * 1024,
            staging_ttl: Duration::from_secs(300),
            promote_hits: 3,
            promote_window: Duration::from_secs(600),
            hot_threshold: 8,
            decay_half_life: Duration::from_secs(24 * 3600),
            preload_on_start: true,
        }
    }
}

/// Observability snapshot for the settings dashboard and the memlog line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TierStats {
    pub hot_bytes: u64,
    pub hot_entries: u64,
    pub hot_hits: u64,
    pub hot_misses: u64,
    pub staging_bytes: u64,
    pub staging_entries: u64,
    pub staging_hits: u64,
    pub evictions: u64,
    pub promotions: u64,
}

/// One entry's decayed heat bookkeeping.
#[derive(Debug, Clone, Copy)]
struct Heat {
    /// Decayed hit count, scaled by 256 (fixed-point) so halving keeps
    /// sub-unit resolution.
    score: u32,
    /// When `score` was last computed.
    at: Instant,
    /// Hits inside the current promotion window.
    window_hits: u32,
    window_started: Instant,
}

/// The wall-clock half-life decay for a ×256 fixed-point heat score —
/// the formula `Heat::decay` books, extracted so the engine's
/// persistence path (the periodic snapshot it writes through
/// `Store::replace_heat`) applies EXACTLY the same decay the in-memory
/// tier cache does: a score persisted, then replayed after a restart,
/// must land where it would have had the cache never stopped running.
/// Zero `half_life` (decay disabled) or zero `elapsed` returns the
/// score unchanged.
pub(crate) fn decay_score(score: u32, elapsed: Duration, half_life: Duration) -> u32 {
    if elapsed.is_zero() || half_life.is_zero() {
        return score;
    }
    // Halve for every full half-life; a partial half-life halves
    // proportionally (score is fixed-point x256 so this stays >0 for
    // any realistic count until it truly rounds away). ROUND, never
    // truncate: 256×(1−ε) truncating to 255 read as score 0 (the
    // promotion gate bug caught by rescan_flushes_then_preheats).
    let halves = elapsed.as_secs_f64() / half_life.as_secs_f64();
    let factor = (0.5f64).powf(halves);
    ((score as f64) * factor).round() as u32
}

impl Heat {
    fn new(now: Instant) -> Heat {
        Heat {
            score: 0,
            at: now,
            window_hits: 1,
            window_started: now,
        }
    }

    /// Decays `score` to `now` (called before any read/write of it).
    /// The zero guards stay here (not only inside [`decay_score`]): an
    /// early return also skips the `at` re-stamp, keeping pre-extraction
    /// behavior byte-identical when decay is disabled.
    fn decay(&mut self, now: Instant, half_life: Duration) {
        let elapsed = now.saturating_duration_since(self.at);
        if elapsed.is_zero() || half_life.is_zero() {
            return;
        }
        self.score = decay_score(self.score, elapsed, half_life);
        self.at = now;
    }

    fn record_hit(&mut self) {
        self.score = self.score.saturating_add(256);
    }
}

/// The generic tier cache. `K` is the entry key, `V` the cached value;
/// the caller supplies each value's byte cost at insert.
pub struct TierCache<K, V> {
    policy: MemoryPolicy,
    hot: HashMap<K, (V, u64)>,
    staging: HashMap<K, (V, u64, Instant)>, // value, bytes, expires_at
    heat: HashMap<K, Heat>,
    stats: TierStats,
    /// Injected clock (tests control it); production uses [`Instant::now`].
    now_fn: fn() -> Instant,
}

impl<K: std::hash::Hash + Eq + Clone, V: Clone> TierCache<K, V> {
    pub fn new(policy: MemoryPolicy) -> TierCache<K, V> {
        TierCache::with_clock(policy, Instant::now)
    }

    /// Test seam: deterministic time.
    pub fn with_clock(policy: MemoryPolicy, now_fn: fn() -> Instant) -> TierCache<K, V> {
        TierCache {
            policy,
            hot: HashMap::new(),
            staging: HashMap::new(),
            heat: HashMap::new(),
            stats: TierStats::default(),
            now_fn,
        }
    }

    fn now(&self) -> Instant {
        (self.now_fn)()
    }

    /// Hot-tier lookup (a hit also refreshes heat).
    pub fn get_hot(&mut self, key: &K) -> Option<&V> {
        if self.hot.contains_key(key) {
            self.record_hit(key);
            self.stats.hot_hits += 1;
            Some(&self.hot.get(key).expect("just checked").0)
        } else {
            self.stats.hot_misses += 1;
            None
        }
    }

    /// Staging lookup; an expired entry is dropped (cold again).
    /// Staging lookup. A hit REFRESHES the TTL (a continuously-accessed
    /// entry must not fall out mid-use — review fix #9) but does NOT
    /// record heat: the ladders call [`TierCache::record_hit`] themselves,
    /// and double-counting made promotion trigger at half the configured
    /// threshold (review fix #4). An expired entry drops out (and its
    /// orphan Heat bookkeeping with it — review fix #8).
    pub fn get_staging(&mut self, key: &K) -> Option<V> {
        let now = self.now();
        let expired = self
            .staging
            .get(key)
            .is_some_and(|(_, _, expires_at)| now >= *expires_at);
        if expired {
            if let Some((_, bytes, _)) = self.staging.remove(key) {
                self.stats.staging_bytes = self.stats.staging_bytes.saturating_sub(bytes);
                self.drop_orphan_heat(key);
            }
        }
        if let Some((value, bytes, expires_at)) = self.staging.get_mut(key) {
            self.stats.staging_hits += 1;
            let v = value.clone();
            *expires_at = now + self.policy.staging_ttl; // TTL refresh on hit
            let _ = bytes;
            Some(v)
        } else {
            None
        }
    }

    /// Records a hit: updates heat ONLY for keys already holding a Heat
    /// entry (staging/hot residents) — the Heat map stays bounded by the
    /// tier budgets, never the access stream (a day of millions of
    /// one-off keys creates nothing). Returns the key's decayed score,
    /// 0 when it holds no entry.
    pub fn record_hit(&mut self, key: &K) -> u32 {
        let now = self.now();
        let policy = self.policy;
        let Some(heat) = self.heat.get_mut(key) else {
            return 0;
        };
        heat.decay(now, policy.decay_half_life);
        heat.record_hit();
        // Sliding promotion window: reset when the previous window aged
        // out, else count within it.
        if now.saturating_duration_since(heat.window_started) > policy.promote_window {
            heat.window_hits = 1;
            heat.window_started = now;
        } else {
            heat.window_hits += 1;
        }
        heat.score / 256
    }

    /// Cold → staging placement (caller read storage and produced the
    /// value). Enforces the staging budget by evicting expired-then-
    /// lowest-heat entries.
    pub fn put_staging(&mut self, key: K, value: V, bytes: u64) {
        let now = self.now();
        // Expire first — that alone may free the budget.
        self.expire_staging(now);
        while self.stats.staging_bytes + bytes > self.policy.staging_budget_bytes
            && !self.staging.is_empty()
        {
            self.evict_lowest_staging();
        }
        if self.stats.staging_bytes + bytes > self.policy.staging_budget_bytes {
            // The value alone exceeds the budget: refuse to hold it.
            return;
        }
        let expires_at = now + self.policy.staging_ttl;
        if let Some((_, old_bytes, _)) =
            self.staging.insert(key.clone(), (value, bytes, expires_at))
        {
            self.stats.staging_bytes = self.stats.staging_bytes.saturating_sub(old_bytes);
        } else {
            self.stats.staging_bytes += bytes;
        }
        self.heat.entry(key).or_insert_with(|| Heat::new(now));
    }

    /// Promotes an entry to the hot tier when its decayed score has
    /// reached the threshold (checked after every recorded hit by the
    /// engine; kept explicit here so tests can drive it directly).
    pub fn maybe_promote_to_hot(&mut self, key: &K, value: V, bytes: u64) -> bool {
        let now = self.now();
        let policy = self.policy;
        let (score, window_hits) = match self.heat.get_mut(key) {
            Some(heat) => {
                heat.decay(now, policy.decay_half_life);
                (heat.score / 256, heat.window_hits)
            }
            // No Heat entry = the key is not a tier resident (a cold
            // key's score is 0 by definition — it cannot promote).
            None => return false,
        };
        // Burst gate (review fix #2): promotion needs BOTH the decayed
        // score and ≥ promote_hits hits inside the promote window — the
        // 晋升命中次数/晋升窗口 knobs the settings UI ships.
        if score < self.policy.hot_threshold || window_hits < self.policy.promote_hits {
            return false;
        }
        while self.stats.hot_bytes + bytes > self.policy.hot_budget_bytes && !self.hot.is_empty() {
            self.evict_lowest_hot();
        }
        if self.stats.hot_bytes + bytes > self.policy.hot_budget_bytes {
            return false; // larger than the whole budget
        }
        if let Some((_, old_bytes)) = self.hot.insert(key.clone(), (value, bytes)) {
            self.stats.hot_bytes = self.stats.hot_bytes.saturating_sub(old_bytes);
        } else {
            self.stats.hot_bytes += bytes;
        }
        // Hot-tier residency supersedes staging.
        if let Some((_, s_bytes, _)) = self.staging.remove(key) {
            self.stats.staging_bytes = self.stats.staging_bytes.saturating_sub(s_bytes);
        }
        self.stats.promotions += 1;
        true
    }

    /// Direct hot insert for startup preheat (persisted heat already
    /// earned the entry; no threshold re-check).
    /// Promotes a STAGING resident to hot by MOVING its value — no
    /// refetch. Returns false when the score is below threshold, the key
    /// is not staged, or the budget refuses the entry.
    pub fn promote_from_staging(&mut self, key: &K) -> bool {
        let now = self.now();
        let policy = self.policy;
        let (score, window_hits) = match self.heat.get_mut(key) {
            Some(heat) => {
                heat.decay(now, policy.decay_half_life);
                (heat.score / 256, heat.window_hits)
            }
            None => return false,
        };
        // Same dual gate as maybe_promote_to_hot (review fix #2).
        if score < self.policy.hot_threshold || window_hits < self.policy.promote_hits {
            return false;
        }
        let Some((value, bytes, _)) = self.staging.remove(key) else {
            return false;
        };
        self.stats.staging_bytes = self.stats.staging_bytes.saturating_sub(bytes);
        while self.stats.hot_bytes + bytes > self.policy.hot_budget_bytes && !self.hot.is_empty() {
            self.evict_lowest_hot();
        }
        if self.stats.hot_bytes + bytes > self.policy.hot_budget_bytes {
            // Budget refuses: back to staging rather than dropping.
            let expires_at = now + self.policy.staging_ttl;
            self.staging.insert(key.clone(), (value, bytes, expires_at));
            self.stats.staging_bytes += bytes;
            return false;
        }
        if let Some((_, old_bytes)) = self.hot.insert(key.clone(), (value, bytes)) {
            self.stats.hot_bytes = self.stats.hot_bytes.saturating_sub(old_bytes);
        } else {
            self.stats.hot_bytes += bytes;
        }
        self.stats.promotions += 1;
        true
    }

    /// Snapshot of the hot tier's contents (rescan-preheat path).
    pub fn hot_snapshot(&self) -> Vec<(K, V, u64)> {
        self.hot
            .iter()
            .map(|(k, (v, bytes))| (k.clone(), v.clone(), *bytes))
            .collect()
    }

    /// Direct hot insert carrying a persisted heat score — the
    /// restart-backfill path (the snapshot's ×256 fixed-point score
    /// re-enters with the entry; no threshold re-check). Budget eviction
    /// is [`TierCache::put_hot_direct`]'s. Entering hot SUPERSEDES the
    /// staging twin (staging_bytes 扣减 — the promote paths' pattern, so
    /// one key never bills both tiers). Heat merge: no entry seeds
    /// `Heat { score, at: now, window_hits: 1, window_started: now }`;
    /// an existing entry decays to now first, then keeps
    /// `max(existing, score)` — a persisted snapshot never clobbers a
    /// higher live score — with the promotion window fields preserved.
    pub fn put_hot_direct_with_score(&mut self, key: K, value: V, bytes: u64, score: u32) {
        while self.stats.hot_bytes + bytes > self.policy.hot_budget_bytes && !self.hot.is_empty() {
            self.evict_lowest_hot();
        }
        if self.stats.hot_bytes + bytes > self.policy.hot_budget_bytes {
            return;
        }
        if let Some((_, old_bytes)) = self.hot.insert(key.clone(), (value, bytes)) {
            self.stats.hot_bytes = self.stats.hot_bytes.saturating_sub(old_bytes);
        } else {
            self.stats.hot_bytes += bytes;
        }
        // Hot-tier residency supersedes staging (无双份计账).
        if let Some((_, s_bytes, _)) = self.staging.remove(&key) {
            self.stats.staging_bytes = self.stats.staging_bytes.saturating_sub(s_bytes);
        }
        let now = self.now();
        let policy = self.policy;
        match self.heat.get_mut(&key) {
            Some(heat) => {
                heat.decay(now, policy.decay_half_life);
                heat.score = heat.score.max(score);
            }
            None => {
                self.heat.insert(
                    key,
                    Heat {
                        score,
                        at: now,
                        window_hits: 1,
                        window_started: now,
                    },
                );
            }
        }
    }

    /// Direct hot insert for startup preheat (persisted heat already
    /// earned the entry; no threshold re-check). The score-0 special
    /// case of [`TierCache::put_hot_direct_with_score`]: a fresh entry
    /// seeds the same zero-score Heat as before, an existing entry
    /// keeps its decayed live score (max with 0 is a no-op).
    pub fn put_hot_direct(&mut self, key: K, value: V, bytes: u64) {
        self.put_hot_direct_with_score(key, value, bytes, 0);
    }

    /// Persistence snapshot of the whole heat map: every entry is first
    /// decayed to now (the side effect re-stamps each `Heat.at` —
    /// correct and harmless: the next decay measures from this
    /// snapshot), then the entries whose floored score
    /// `raw / 256 >= floor_hits` come back as `(key, raw)` pairs,
    /// raw-score descending (the persist path keeps the head of this
    /// order). The floor is the SAME `hot_threshold` gate the backfill
    /// side applies, so a one-touch entry (raw 256) never round-trips
    /// through the store.
    pub fn heat_above_floor(&mut self, floor_hits: u32) -> Vec<(K, u32)> {
        let now = self.now();
        let policy = self.policy;
        for heat in self.heat.values_mut() {
            heat.decay(now, policy.decay_half_life);
        }
        let mut out: Vec<(K, u32)> = self
            .heat
            .iter()
            .filter(|(_, heat)| heat.score / 256 >= floor_hits)
            .map(|(key, heat)| (key.clone(), heat.score))
            .collect();
        out.sort_by_key(|(_, raw)| std::cmp::Reverse(*raw));
        out
    }

    /// The hot tier's residents as `(key, decayed raw score)` — the
    /// rescan flush's carry list: a rescan hands each previously-hot id
    /// its score back instead of zeroing it. Decays to now first (same
    /// side effect as [`TierCache::heat_above_floor`]).
    pub fn hot_ids_with_scores(&mut self) -> Vec<(K, u32)> {
        let now = self.now();
        let policy = self.policy;
        for heat in self.heat.values_mut() {
            heat.decay(now, policy.decay_half_life);
        }
        self.hot
            .keys()
            .filter_map(|key| self.heat.get(key).map(|heat| (key.clone(), heat.score)))
            .collect()
    }

    /// Drops one entry everywhere (invalidation hook).
    pub fn invalidate(&mut self, key: &K) {
        if let Some((_, bytes)) = self.hot.remove(key) {
            self.stats.hot_bytes = self.stats.hot_bytes.saturating_sub(bytes);
        }
        if let Some((_, bytes, _)) = self.staging.remove(key) {
            self.stats.staging_bytes = self.stats.staging_bytes.saturating_sub(bytes);
        }
        self.heat.remove(key);
    }

    pub fn clear(&mut self) {
        self.hot.clear();
        self.staging.clear();
        self.heat.clear();
        self.stats = TierStats {
            hot_hits: self.stats.hot_hits,
            hot_misses: self.stats.hot_misses,
            staging_hits: self.stats.staging_hits,
            evictions: self.stats.evictions,
            promotions: self.stats.promotions,
            ..TierStats::default()
        };
    }

    /// Live-applies a new policy, immediately enforcing the new budgets.
    pub fn set_policy(&mut self, policy: MemoryPolicy) {
        self.policy = policy;
        self.enforce_budgets();
    }

    pub fn policy(&self) -> MemoryPolicy {
        self.policy
    }

    pub fn stats(&self) -> TierStats {
        let mut stats = self.stats;
        stats.hot_entries = self.hot.len() as u64;
        stats.staging_entries = self.staging.len() as u64;
        stats
    }

    fn enforce_budgets(&mut self) {
        let now = self.now();
        self.expire_staging(now);
        while self.stats.staging_bytes > self.policy.staging_budget_bytes
            && !self.staging.is_empty()
        {
            self.evict_lowest_staging();
        }
        while self.stats.hot_bytes > self.policy.hot_budget_bytes && !self.hot.is_empty() {
            self.evict_lowest_hot();
        }
    }

    fn expire_staging(&mut self, now: Instant) {
        let expired: Vec<K> = self
            .staging
            .iter()
            .filter(|(_, (_, _, expires_at))| now >= *expires_at)
            .map(|(k, _)| k.clone())
            .collect();
        for key in expired {
            if let Some((_, bytes, _)) = self.staging.remove(&key) {
                self.stats.staging_bytes = self.stats.staging_bytes.saturating_sub(bytes);
                self.drop_orphan_heat(&key);
            }
        }
    }

    /// Removes the key's Heat bookkeeping once it holds residency in
    /// neither tier — the Heat map stays bounded by the tier budgets.
    fn drop_orphan_heat(&mut self, key: &K) {
        if !self.hot.contains_key(key) && !self.staging.contains_key(key) {
            self.heat.remove(key);
        }
    }

    fn evict_lowest_staging(&mut self) {
        let now = self.now();
        let policy = self.policy;
        let victim = self
            .staging
            .keys()
            .min_by_key(|k| {
                self.heat
                    .get(*k)
                    .map(|h| {
                        let mut h = *h;
                        h.decay(now, policy.decay_half_life);
                        h.score
                    })
                    .unwrap_or(0)
            })
            .cloned();
        if let Some(k) = victim {
            if let Some((_, bytes, _)) = self.staging.remove(&k) {
                self.stats.staging_bytes = self.stats.staging_bytes.saturating_sub(bytes);
                self.stats.evictions += 1;
                self.drop_orphan_heat(&k);
            }
        }
    }

    fn evict_lowest_hot(&mut self) {
        let now = self.now();
        let policy = self.policy;
        let victim = self
            .hot
            .keys()
            .min_by_key(|k| {
                self.heat
                    .get(*k)
                    .map(|h| {
                        let mut h = *h;
                        h.decay(now, policy.decay_half_life);
                        h.score
                    })
                    .unwrap_or(0)
            })
            .cloned();
        if let Some(k) = victim {
            if let Some((_, bytes)) = self.hot.remove(&k) {
                self.stats.hot_bytes = self.stats.hot_bytes.saturating_sub(bytes);
                self.stats.evictions += 1;
                self.drop_orphan_heat(&k);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests need a controllable `fn() -> Instant`; a plain fn pointer
    // cannot capture, so the current time parks in a thread_local the
    // test clock reads.
    thread_local! {
        static TEST_NOW: std::cell::RefCell<Option<Instant>> = const { std::cell::RefCell::new(None) };
    }
    fn test_now() -> Instant {
        TEST_NOW.with(|t| t.borrow().unwrap_or_else(Instant::now))
    }
    fn set_test_now(t: Instant) {
        TEST_NOW.with(|tl| *tl.borrow_mut() = Some(t));
    }

    fn cache(policy: MemoryPolicy) -> TierCache<String, String> {
        TierCache::with_clock(policy, test_now)
    }

    #[test]
    fn tiers_capacity_never_exceeded_under_flood() {
        let t0 = Instant::now();
        set_test_now(t0);
        let policy = MemoryPolicy {
            hot_budget_bytes: 1_000,
            staging_budget_bytes: 1_000,
            ..MemoryPolicy::default()
        };
        let mut c = cache(policy);
        // 10x the budget through staging.
        for i in 0..10 {
            let key = format!("k{i}");
            c.put_staging(key.clone(), "v".repeat(200), 200);
        }
        assert!(c.stats().staging_bytes <= 1_000, "staging over budget");
        // And through hot (threshold 0 so every RESIDENT qualifies —
        // promotion requires tier residency, so stage each first).
        let policy0 = MemoryPolicy {
            hot_budget_bytes: 1_000,
            hot_threshold: 0,
            promote_hits: 1, // every staged entry qualifies immediately
            ..MemoryPolicy::default()
        };
        c.set_policy(policy0);
        for i in 0..10 {
            let key = format!("h{i}");
            let v = "v".repeat(200);
            c.put_staging(key.clone(), v.clone(), 200);
            assert!(c.maybe_promote_to_hot(&key, v, 200));
        }
        assert!(c.stats().hot_bytes <= 1_000, "hot over budget");
        assert!(c.stats().evictions > 0, "overflow must evict");
    }

    #[test]
    fn promotion_requires_both_score_and_burst() {
        let t0 = Instant::now();
        set_test_now(t0);
        // The burst rule (review fix #2): staging→hot needs the decayed
        // score ≥ hot_threshold AND ≥ promote_hits hits inside the
        // promote_window. Without the burst gate, a single access whose
        // score somehow crossed the threshold would jump straight to hot
        // — the knob the settings UI ships as 晋升命中次数.
        let policy = MemoryPolicy {
            hot_threshold: 1,
            promote_hits: 5,
            promote_window: Duration::from_secs(600),
            ..MemoryPolicy::default()
        };
        let mut c = cache(policy);
        c.put_staging("a".into(), "v".into(), 10);
        for _ in 0..3 {
            c.record_hit(&"a".to_string());
        }
        // put_staging seeds window_hits at 1; +3 record hits = 4 < 5.
        assert!(
            !c.maybe_promote_to_hot(&"a".to_string(), "v".to_string(), 10),
            "score crossed but 4 window hits < promote_hits 5"
        );
        c.record_hit(&"a".to_string()); // 5th window hit
        assert!(
            c.maybe_promote_to_hot(&"a".to_string(), "v".to_string(), 10),
            "score AND burst both satisfied"
        );
    }

    #[test]
    fn promotion_cold_to_staging_to_hot() {
        let t0 = Instant::now();
        set_test_now(t0);
        // First access loads the value into STAGING (a displayed page
        // needs its data — cold→staging is unconditional); repeated hits
        // then build the heat that promotes it into the hot tier.
        let policy = MemoryPolicy {
            hot_threshold: 2,
            promote_hits: 3,
            ..MemoryPolicy::default()
        };
        let mut c = cache(policy);
        c.put_staging("a".into(), "value".into(), 10);
        for _ in 0..3 {
            c.record_hit(&"a".to_string());
        }
        let promoted = c.maybe_promote_to_hot(&"a".to_string(), "value".to_string(), 10);
        assert!(promoted, "3 hits (score 3) cross threshold 2");
        assert!(c.get_hot(&"a".to_string()).is_some());
        // A key with no residency (pure cold) has no heat and cannot
        // promote — and recording hits on it allocates nothing.
        let before = c.stats();
        for _ in 0..5 {
            c.record_hit(&"stranger".to_string());
        }
        assert!(!c.maybe_promote_to_hot(&"stranger".to_string(), "v".to_string(), 10));
        assert_eq!(c.stats().hot_entries, before.hot_entries);
    }

    #[test]
    fn ttl_expiry_returns_to_cold() {
        let t0 = Instant::now();
        set_test_now(t0);
        let policy = MemoryPolicy {
            staging_ttl: Duration::from_secs(60),
            ..MemoryPolicy::default()
        };
        let mut c = cache(policy);
        c.put_staging("k".into(), "v".into(), 10);
        assert!(c.get_staging(&"k".to_string()).is_some());
        set_test_now(t0 + Duration::from_secs(61));
        assert!(
            c.get_staging(&"k".to_string()).is_none(),
            "expired entry is cold again"
        );
    }

    #[test]
    fn decay_demotes_yesterdays_hot() {
        let t0 = Instant::now();
        set_test_now(t0);
        let policy = MemoryPolicy {
            hot_threshold: 4,
            decay_half_life: Duration::from_secs(3600),
            ..MemoryPolicy::default()
        };
        let mut c = cache(policy);
        c.put_staging("old".into(), "v".into(), 10);
        for _ in 0..8 {
            c.record_hit(&"old".to_string());
        }
        // Two half-lives later the score quartered: 8 → 2 < threshold 4.
        set_test_now(t0 + Duration::from_secs(7200));
        let score = c.record_hit(&"old".to_string());
        assert!(score < 4, "decayed score {score} below threshold");
        assert!(!c.maybe_promote_to_hot(&"old".to_string(), "v".into(), 10));
    }

    #[test]
    fn policy_live_apply_resizes_tiers() {
        let t0 = Instant::now();
        set_test_now(t0);
        let mut c = cache(MemoryPolicy {
            hot_budget_bytes: 10_000,
            hot_threshold: 0,
            ..MemoryPolicy::default()
        });
        for i in 0..10 {
            c.put_hot_direct(format!("k{i}"), "v".repeat(900), 900);
        }
        assert_eq!(c.stats().hot_bytes, 9_000);
        c.set_policy(MemoryPolicy {
            hot_budget_bytes: 1_000,
            hot_threshold: 0,
            ..MemoryPolicy::default()
        });
        assert!(
            c.stats().hot_bytes <= 1_000,
            "live apply must evict to budget"
        );
    }

    #[test]
    fn invalidate_drops_everywhere() {
        let t0 = Instant::now();
        set_test_now(t0);
        let mut c = cache(MemoryPolicy {
            hot_threshold: 0,
            ..MemoryPolicy::default()
        });
        c.put_staging("k".into(), "v".into(), 10);
        c.maybe_promote_to_hot(&"k".to_string(), "v".to_string(), 10);
        c.invalidate(&"k".to_string());
        assert!(c.get_hot(&"k".to_string()).is_none());
        assert!(c.get_staging(&"k".to_string()).is_none());
    }

    #[test]
    fn decay_score_rounds_never_truncates() {
        let half = Duration::from_secs(24 * 3600);
        // A 1ms-old score sits at 256×(1−ε): ROUND keeps the unit (the
        // promotion gate reads raw / 256 — truncation turned 256 into
        // 255, which floored to score 0 and silently broke promotion,
        // the rescan_flushes_then_preheats bug).
        assert_eq!(decay_score(256, Duration::from_millis(1), half), 256);
        // Zero elapsed: nothing has decayed.
        assert_eq!(decay_score(256, Duration::ZERO, half), 256);
        // Exactly two half-lives: 1024 → 512 → 256, on the mark.
        assert_eq!(decay_score(1024, 2 * half, half), 256);
        // Zero half-life = decay disabled: the score passes through.
        assert_eq!(
            decay_score(100, Duration::from_secs(3600), Duration::ZERO),
            100
        );
    }

    #[test]
    fn with_score_supersedes_staging_and_merges_max() {
        let t0 = Instant::now();
        set_test_now(t0);
        let mut c = cache(MemoryPolicy::default());
        c.put_staging("k".into(), "v".into(), 10);
        // Live score 2 < the incoming persisted score 8×256 (带分回填:
        // the snapshot's fixed-point score re-enters the entry).
        c.record_hit(&"k".to_string());
        c.record_hit(&"k".to_string());
        c.put_hot_direct_with_score("k".into(), "v".into(), 10, 8 * 256);
        assert!(c.get_hot(&"k".to_string()).is_some());
        assert_eq!(c.stats().staging_entries, 0, "入热接管 staging 孪生");
        assert_eq!(c.stats().hot_bytes, 10, "无双份计账");
        // The seeded score is live: 2048/256 = 8, plus this hit ≥ 8.
        assert!(c.record_hit(&"k".to_string()) >= 8);
    }

    #[test]
    fn with_score_does_not_clobber_higher_live() {
        let t0 = Instant::now();
        set_test_now(t0);
        let mut c = cache(MemoryPolicy::default());
        c.put_staging("k".into(), "v".into(), 10);
        for _ in 0..20 {
            c.record_hit(&"k".to_string());
        }
        // Live score 20×256 > the incoming 8×256: max keeps the live
        // score — a persisted snapshot must never LOWER a hot entry.
        c.put_hot_direct_with_score("k".into(), "v".into(), 10, 8 * 256);
        assert!(
            c.record_hit(&"k".to_string()) >= 20,
            "max merge keeps the higher live score"
        );
    }
}
