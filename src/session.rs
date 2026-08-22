//! In-memory conversation store for `previous_response_id` handling.
//!
//! The Responses API lets clients continue a conversation by sending
//! `previous_response_id` in the next request. Upstream Chat-Completions
//! providers are stateless and don't understand that field, and switching
//! providers mid-conversation would otherwise lose all context. This module
//! is the proxy's own store: each `response_id` maps to the **complete**
//! conversation context at that point in time, stored as Responses-format
//! items (format-agnostic — the same items work for both `chat` and
//! `responses` upstreams, spec §8.2).
//!
//! ## Storage model: full-context snapshots
//!
//! Every turn stores the whole conversation (prior items + new input + new
//! output), not incremental deltas — O(n²) total storage, intentionally
//! simple for the MVP (spec §8.2). Memory is bounded by three limits:
//!
//! * **TTL** — sessions idle longer than `ttl` are evicted. The TTL is
//!   *sliding*: every `get` hit and every `save` refreshes `last_used_at`,
//!   so an actively-used conversation is never evicted for idleness.
//! * **LRU count** — beyond `max_sessions`, the least-recently-used session
//!   is evicted; `get` promotes a session's recency.
//! * **Memory budget** — beyond `max_memory_mb` (a rough string-length
//!   estimate), the oldest sessions are evicted.
//!
//! ## Graceful degradation
//!
//! A miss (expired, evicted, or the proxy restarted) is **not** an error:
//! the caller in `proxy.rs` logs a warning and continues with the new input
//! only. The conversation may lose context, but it never crashes (spec
//! §8.3).
//!
//! ## Concurrency & sharing
//!
//! [`SessionStore`] is cheap to clone: it wraps an
//! `Arc<RwLock<SessionState>>`, so all clones share one store. The streaming
//! handler clones it into a spawned task and writes once the stream
//! finishes.
//!
//! ## Response IDs
//!
//! IDs are proxy-generated `resp_<uuid_v4_simple>` for chat-format
//! providers ([`SessionStore::new_response_id`]). For `responses`-format
//! passthrough providers, sessions are keyed by the upstream's own response
//! ID instead (see AGENTS.md convention #4).

use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;
use uuid::Uuid;

/// In-memory session store for previous_response_id handling.
/// Stores conversation context (Responses-format items) per response_id.
///
/// Clone to share: the inner `Arc<RwLock<SessionState>>` means every clone
/// observes the same map and writes serialize through the single write
/// lock. Note that `get` also takes the *write* lock because it must update
/// the LRU recency timestamp.
#[derive(Clone)]
pub struct SessionStore {
    state: Arc<RwLock<SessionState>>,
}

/// One stored conversation: the full item list plus the LRU recency stamp.
struct SessionEntry {
    /// Complete conversation context as Responses-format items.
    items: Vec<Value>,
    /// Last access time; drives both the sliding TTL and LRU eviction.
    /// Refreshed on every `save` and on every `get` hit.
    last_used_at: SystemTime,
}

/// The shared mutable store behind [`SessionStore`].
///
/// `sessions` maps a response ID to its full-context entry. The remaining
/// fields are the enforced limits, copied from the config's `[session]`
/// section at construction time.
struct SessionState {
    /// response_id → conversation entry.
    sessions: HashMap<String, SessionEntry>,
    /// Idle retention window (sliding; reset on each get/save).
    ttl: Duration,
    /// Maximum number of cached sessions (LRU eviction beyond this).
    max_sessions: usize,
    /// Memory budget in bytes (LRU eviction beyond this).
    max_memory_bytes: usize,
}

impl SessionStore {
    /// Create a new store with the given limits.
    ///
    /// `ttl_hours` is converted to a [`Duration`] and `max_memory_mb` to
    /// bytes; both conversions happen once here. A `max_sessions` or
    /// `max_memory_mb` of `0` effectively disables caching.
    pub fn new(ttl_hours: u64, max_sessions: usize, max_memory_mb: usize) -> Self {
        Self {
            state: Arc::new(RwLock::new(SessionState {
                sessions: HashMap::new(),
                ttl: Duration::from_secs(ttl_hours * 3600),
                max_sessions,
                max_memory_bytes: max_memory_mb * 1024 * 1024,
            })),
        }
    }

    /// Generate a new unique response ID.
    ///
    /// Returns `resp_<uuid_v4_simple>`. Called by the handler before a
    /// response is created; the ID is returned to the client so it can be
    /// sent back as `previous_response_id` on the next turn. UUID v4 makes
    /// collisions practically impossible.
    pub fn new_response_id(&self) -> String {
        format!("resp_{}", Uuid::new_v4().simple())
    }

    /// Store conversation items under a response_id.
    ///
    /// Overwrites any existing entry with the same ID (IDs are unique in
    /// practice). The stored entry is the **complete** context snapshot for
    /// that turn, as assembled by the handler.
    ///
    /// ### Oversized-session skip
    ///
    /// If a single session's estimated size exceeds the *whole* memory
    /// budget, it is **not cached at all** (warning logged, spec §8.5).
    /// Such a conversation would immediately evict everything else in the
    /// store, so it is cheaper to simply not cache it — the next turn then
    /// degrades gracefully on a miss.
    ///
    /// After the insert, `enforce_limits` prunes expired and oldest
    /// sessions so the store never exceeds its caps.
    pub async fn save(&self, id: String, items: Vec<Value>) {
        let mut state = self.state.write().await;
        // Rough size estimate: sum of the serialized length of each item.
        // Not the exact JSON size, but cheap and good enough for a budget.
        let estimated_bytes: usize = items.iter().map(|v| v.to_string().len()).sum();
        // Skip if single session exceeds total budget
        if estimated_bytes > state.max_memory_bytes {
            tracing::warn!(
                "session {id} is {estimated_bytes} bytes, exceeds memory limit, not caching"
            );
            return;
        }
        state.sessions.insert(
            id,
            SessionEntry {
                items,
                // Inserting is itself a "use": start the sliding TTL clock now.
                last_used_at: SystemTime::now(),
            },
        );
        state.enforce_limits();
    }

    /// Retrieve conversation items for a response_id.
    /// Returns None if not found (expired, evicted, or never stored).
    ///
    /// A hit **promotes** the session in the LRU order (and resets its
    /// sliding TTL) by refreshing `last_used_at`, then returns a cloned copy
    /// of the items so the caller can't mutate the stored state. The clone
    /// is a `Vec<Value>` per turn — cheap enough for the MVP.
    ///
    /// Returns `None` on a miss; the caller degrades gracefully (new input
    /// only, warning logged) rather than failing the request.
    pub async fn get(&self, id: &str) -> Option<Vec<Value>> {
        let mut state = self.state.write().await;
        if let Some(entry) = state.sessions.get_mut(id) {
            entry.last_used_at = SystemTime::now();
            let items = entry.items.clone();
            // Count/memory limits are enforced on every save, so the only
            // read-path concern is time-based expiry. Avoid re-serializing
            // all items (estimated_bytes) on the hot path.
            state.remove_expired();
            tracing::debug!("session hit {id}");
            Some(items)
        } else {
            tracing::debug!("session miss {id}");
            None
        }
    }

    /// Remove expired sessions.
    ///
    /// Public entry point for the background cleanup task (`proxy.rs` runs
    /// it hourly, spec §8.5). Eviction is otherwise lazy — `save` and `get`
    /// also prune on their own — so this is a belt-and-suspenders sweep for
    /// sessions that expire while the store is otherwise idle.
    pub async fn cleanup(&self) {
        let mut state = self.state.write().await;
        state.remove_expired();
    }
}

impl SessionState {
    /// Enforce all capacity limits after a mutation (called on every save).
    ///
    /// Order matters: expired sessions are dropped first (freeing both
    /// slots and bytes), then the count cap, then the byte budget. The
    /// byte-budget loop stops at one remaining session — a single huge
    /// session is allowed to stay rather than evict itself (its size was
    /// already checked at insert time by `save`).
    fn enforce_limits(&mut self) {
        self.remove_expired();
        // LRU by last_used_at
        while self.sessions.len() > self.max_sessions {
            self.remove_oldest();
        }
        // Memory limit (rough estimate by string length)
        while self.estimated_bytes() > self.max_memory_bytes && self.sessions.len() > 1 {
            self.remove_oldest();
        }
    }

    /// Total estimated memory usage across all sessions.
    ///
    /// Sums the serialized length of every item in every session. String
    /// length is a cheap upper-ish bound on the JSON values' heap usage —
    /// deliberately simple, no allocator introspection.
    fn estimated_bytes(&self) -> usize {
        self.sessions
            .values()
            .map(|e| e.items.iter().map(|v| v.to_string().len()).sum::<usize>())
            .sum()
    }

    /// Drop sessions whose `last_used_at` is older than the TTL window.
    ///
    /// `cutoff = now - ttl`; anything touched before that is gone. Because
    /// `last_used_at` refreshes on every get/save, this implements a
    /// **sliding** TTL — an active conversation is never evicted for
    /// idleness. Runs lazily from `save`/`get`/`cleanup`; `SystemTime`
    /// arithmetic is wall-clock based, so it also picks up sessions that
    /// crossed the expiry boundary while the process was idle.
    fn remove_expired(&mut self) {
        let cutoff = SystemTime::now() - self.ttl;
        let expired: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, e)| e.last_used_at < cutoff)
            .map(|(k, _)| k.clone())
            .collect();
        for id in expired {
            self.sessions.remove(&id);
        }
    }

    /// Evict the single least-recently-used session.
    ///
    /// Scans all entries for the minimum `last_used_at` and removes it —
    /// O(n) per call, O(n²) worst case for a full purge, fine at this
    /// scale. Used by both the count and byte-budget eviction loops in
    /// `enforce_limits`.
    fn remove_oldest(&mut self) {
        if let Some(id) = self
            .sessions
            .iter()
            .min_by_key(|(_, e)| e.last_used_at)
            .map(|(k, _)| k.clone())
        {
            self.sessions.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn store_and_retrieve() {
        let store = SessionStore::new(168, 256, 512);
        let id = store.new_response_id();
        let items = vec![json!({"type":"message","role":"user","content":"hi"})];
        store.save(id.clone(), items.clone()).await;
        let got = store.get(&id).await;
        assert_eq!(got, Some(items));
    }

    #[tokio::test]
    async fn miss_returns_none() {
        let store = SessionStore::new(168, 256, 512);
        assert!(store.get("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn lru_eviction_by_count() {
        let store = SessionStore::new(168, 2, 512);
        let id1 = store.new_response_id();
        let id2 = store.new_response_id();
        let id3 = store.new_response_id();
        store.save(id1.clone(), vec![json!("a")]).await;
        store.save(id2.clone(), vec![json!("b")]).await;
        store.save(id3.clone(), vec![json!("c")]).await;
        // id1 should be evicted (oldest)
        assert!(store.get(&id1).await.is_none());
        assert!(store.get(&id2).await.is_some());
        assert!(store.get(&id3).await.is_some());
    }

    #[tokio::test]
    async fn response_ids_are_unique() {
        let store = SessionStore::new(168, 256, 512);
        let id1 = store.new_response_id();
        let id2 = store.new_response_id();
        assert_ne!(id1, id2);
        assert!(id1.starts_with("resp_"));
    }

    #[tokio::test]
    async fn lru_promotion_on_get() {
        // A get() touch must protect a session from eviction: with max 2,
        // saving a,b, touching a, then saving c should evict b, not a.
        let store = SessionStore::new(168, 2, 512);
        let (a, b, c) = (
            store.new_response_id(),
            store.new_response_id(),
            store.new_response_id(),
        );
        store.save(a.clone(), vec![json!("a")]).await;
        store.save(b.clone(), vec![json!("b")]).await;
        assert!(store.get(&a).await.is_some()); // promotes a (most recent)
        store.save(c.clone(), vec![json!("c")]).await; // evicts b (least recent)
        assert!(store.get(&a).await.is_some());
        assert!(store.get(&b).await.is_none());
        assert!(store.get(&c).await.is_some());
    }

    #[tokio::test]
    async fn oversized_session_not_cached() {
        let store = SessionStore::new(168, 256, 1); // 1 MiB limit
        let id = store.new_response_id();
        // Create a large item > 1 MiB
        let big = "x".repeat(2 * 1024 * 1024);
        store.save(id.clone(), vec![json!(big)]).await;
        assert!(store.get(&id).await.is_none());
    }
}
