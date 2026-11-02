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
//! - every `get` records a hit in a count-min sketch (fixed ~256KB, never
//!   per-entry bookkeeping);
//! - a key hit `promote_hits` times inside `promote_window` enters
//!   staging (TTL `staging_ttl`, refreshed on each hit);
//! - a key whose decayed hit-count reaches `hot_threshold` enters the hot
//!   tier, which never exceeds `hot_budget` — overflow evicts the
//!   lowest-heat entry;
//! - heat halves every `decay_half_life`; time is injected so tests are
//!   deterministic.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Fixed count-min sketch width. ~256KB: 4 rows × 65,536 counters × u16.
const CMS_ROWS: usize = 4;
const CMS_WIDTH: usize = 65_536;

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
    fn decay(&mut self, now: Instant, half_life: Duration) {
        let elapsed = now.saturating_duration_since(self.at);
        if elapsed.is_zero() || half_life.is_zero() {
            return;
        }
        // Halve for every full half-life; a partial half-life halves
        // proportionally (score is fixed-point x256 so this stays >0 for
        // any realistic count until it truly rounds away).
        let halves = elapsed.as_secs_f64() / half_life.as_secs_f64();
        let factor = (0.5f64).powf(halves);
        self.score = ((self.score as f64) * factor) as u32;
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
    cms: Vec<Vec<u16>>,
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
            cms: vec![vec![0; CMS_WIDTH]; CMS_ROWS],
            stats: TierStats::default(),
            now_fn,
        }
    }

    fn now(&self) -> Instant {
        (self.now_fn)()
    }

    fn cms_bump(&mut self, key: &K) {
        for row in 0..CMS_ROWS {
            let h = self.cms_hash(row, key);
            let cell = &mut self.cms[row][h % CMS_WIDTH];
            *cell = cell.saturating_add(1);
        }
    }

    fn cms_hash(&self, row: usize, key: &K) -> usize {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (row as u64).hash(&mut hasher);
        key.hash(&mut hasher);
        hasher.finish() as usize
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
    pub fn get_staging(&mut self, key: &K) -> Option<V> {
        let now = self.now();
        let expired = self
            .staging
            .get(key)
            .is_some_and(|(_, _, expires_at)| now >= *expires_at);
        if expired {
            if let Some((_, bytes, _)) = self.staging.remove(key) {
                self.stats.staging_bytes = self.stats.staging_bytes.saturating_sub(bytes);
            }
        }
        if self.staging.contains_key(key) {
            self.record_hit(key);
            self.stats.staging_hits += 1;
            self.staging.get(key).map(|(v, _, _)| v.clone())
        } else {
            None
        }
    }

    /// Records a hit: bumps the sketch, updates heat, applies the
    /// promotion rules. Returns the key's current decayed heat score.
    /// Records a hit: bumps the global sketch, and updates heat ONLY for
    /// keys already holding a Heat entry (staging/hot residents) — the
    /// Heat map stays bounded by the tier budgets, never the access
    /// stream (a day of millions of one-off keys creates nothing).
    /// Returns the key's decayed score, 0 when it holds no entry.
    pub fn record_hit(&mut self, key: &K) -> u32 {
        let now = self.now();
        self.cms_bump(key);
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
        let score = match self.heat.get_mut(key) {
            Some(heat) => {
                heat.decay(now, policy.decay_half_life);
                heat.score / 256
            }
            // No Heat entry = the key is not a tier resident (a cold
            // key's score is 0 by definition — it cannot promote).
            None => return false,
        };
        if score < self.policy.hot_threshold {
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
        let score = match self.heat.get_mut(key) {
            Some(heat) => {
                heat.decay(now, policy.decay_half_life);
                heat.score / 256
            }
            None => return false,
        };
        if score < self.policy.hot_threshold {
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

    pub fn put_hot_direct(&mut self, key: K, value: V, bytes: u64) {
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
        let now = self.now();
        self.heat.entry(key).or_insert_with(|| Heat::new(now));
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
}
