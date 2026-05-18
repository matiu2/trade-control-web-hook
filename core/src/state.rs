//! Storage abstraction for replay protection (`seen:<id>`) and instrument
//! cooldowns (`cooldown:<instrument>`).
//!
//! A trait keeps the dispatch logic transport-agnostic so a non-CF deployment
//! (e.g. self-hosted on a home machine) can swap in a file-backed store later
//! without touching the core. The Cloudflare KV implementation lives next to
//! the trait for now; when a second backend lands it'll move behind a feature.

use std::future::Future;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::intent::Action;

/// One active cooldown row in a [`Snapshot`]. `set_at` records when the
/// cooldown was put in place so the operator can see how long ago it
/// started; `expires_at` is when it lapses on its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CooldownEntry {
    pub instrument: String,
    /// Backfilled to `expires_at - hours` when missing (older entries in
    /// live KV predate this field).
    #[serde(default)]
    pub set_at: Option<DateTime<Utc>>,
    pub expires_at: DateTime<Utc>,
}

/// One recently-seen replay-protection id in a [`Snapshot`]. Beyond the
/// id used for replay protection, we also carry the action that landed,
/// when it arrived, and a short outcome string so the `status` view can
/// answer "did this id enter a trade, or get rejected, and when relative
/// to its cooldown?"
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeenEntry {
    pub id: String,
    /// Action that was attempted. Defaults to `Enter` for older entries
    /// in live KV that were written before this field existed.
    #[serde(default = "default_action")]
    pub action: Action,
    /// When the worker recorded the action. None on pre-existing entries.
    #[serde(default)]
    pub seen_at: Option<DateTime<Utc>>,
    /// One-line outcome — e.g. `entered`, `rejected: cooled-down`,
    /// `cooldown-set`, `unlocked`, `prep-set`. Empty for legacy entries.
    #[serde(default)]
    pub outcome: String,
    pub expires_at: DateTime<Utc>,
}

fn default_action() -> Action {
    Action::Enter
}

/// One active "prep" flag row in a [`Snapshot`]. A prep records that a
/// named step (e.g. `break-and-close`) landed for an instrument at a
/// specific time; the `enter` gate checks both presence and order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrepEntry {
    pub instrument: String,
    pub step: String,
    pub set_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// One active "veto" flag row in a [`Snapshot`]. Presence alone is the
/// signal — no timestamp ordering applies on vetos.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VetoEntry {
    pub instrument: String,
    pub name: String,
    pub expires_at: DateTime<Utc>,
}

/// Read-only snapshot of the state store for the `status` action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub now: DateTime<Utc>,
    pub cooldowns: Vec<CooldownEntry>,
    pub recent_seen: Vec<SeenEntry>,
    #[serde(default)]
    pub preps: Vec<PrepEntry>,
    #[serde(default)]
    pub vetos: Vec<VetoEntry>,
}

/// Async storage interface. Implementations are `?Send` because the CF Worker
/// runtime is single-threaded WASM and its KV handle is `!Send`.
pub trait StateStore {
    /// Returns true if `id` has already been recorded as seen.
    fn is_seen(&self, id: &str) -> impl Future<Output = Result<bool, StateError>>;

    /// Mark `id` as seen with a TTL in seconds, recording the action and
    /// outcome that ran on this id so `status` can show what happened.
    fn mark_seen(
        &self,
        id: &str,
        action: Action,
        seen_at: DateTime<Utc>,
        outcome: &str,
        ttl_seconds: u64,
    ) -> impl Future<Output = Result<(), StateError>>;

    /// Delete the `seen:<id>` replay-protection record and prune the
    /// index. Used when a prep is cleared so the operator can re-send
    /// the original prep message without hitting the duplicate-id 409.
    /// Best-effort: succeeds even if the key is already gone.
    fn forget_seen(&self, id: &str) -> impl Future<Output = Result<(), StateError>>;

    /// Returns true if `instrument` is currently under cooldown.
    fn is_cooled_down(&self, instrument: &str) -> impl Future<Output = Result<bool, StateError>>;

    /// Set a cooldown on `instrument` for `hours`. `now` is recorded as
    /// the cooldown's start time so `status` shows how long ago it began.
    fn set_cooldown(
        &self,
        instrument: &str,
        hours: u32,
        now: DateTime<Utc>,
    ) -> impl Future<Output = Result<(), StateError>>;

    /// Clear the cooldown for `instrument`. Returns whether it was set before.
    fn clear_cooldown(&self, instrument: &str) -> impl Future<Output = Result<bool, StateError>>;

    /// Record a named prep step for `instrument` with a TTL. `now` is the
    /// timestamp stored on the flag; the entry-time gate uses it to
    /// enforce ordering across multiple preps. `setter_id` is the
    /// message-id that set this prep, stashed inside the value so
    /// `clear_prep` can also forget that id's `seen:<id>` record —
    /// the operator can then re-send the original prep message
    /// without hitting the replay-protection 409.
    fn set_prep(
        &self,
        instrument: &str,
        step: &str,
        now: DateTime<Utc>,
        ttl_seconds: u64,
        setter_id: &str,
    ) -> impl Future<Output = Result<(), StateError>>;

    /// Return `Some(set_at)` if the prep is currently active, `None`
    /// otherwise (absent or expired).
    fn get_prep(
        &self,
        instrument: &str,
        step: &str,
    ) -> impl Future<Output = Result<Option<DateTime<Utc>>, StateError>>;

    /// Clear a prep flag. Returns `Some(setter_id)` if the prep was
    /// active and recorded a setter id, `Some(String::new())` if it
    /// was active but predates the setter-id wire format, and `None`
    /// if it wasn't set.
    fn clear_prep(
        &self,
        instrument: &str,
        step: &str,
    ) -> impl Future<Output = Result<Option<String>, StateError>>;

    /// Record a named veto for `instrument` with a TTL. Presence alone
    /// is the signal — no timestamp needs storing.
    fn set_veto(
        &self,
        instrument: &str,
        name: &str,
        ttl_seconds: u64,
    ) -> impl Future<Output = Result<(), StateError>>;

    /// Returns true if the veto is currently active.
    fn is_vetoed(
        &self,
        instrument: &str,
        name: &str,
    ) -> impl Future<Output = Result<bool, StateError>>;

    /// Clear a veto flag. Returns whether it was set before.
    fn clear_veto(
        &self,
        instrument: &str,
        name: &str,
    ) -> impl Future<Output = Result<bool, StateError>>;

    /// Return a snapshot of active cooldowns and recent seen ids.
    fn snapshot(&self) -> impl Future<Output = Result<Snapshot, StateError>>;
}

/// Maximum number of recent seen ids retained in the index. Tuning knob;
/// the underlying TTL keys are still authoritative for replay protection.
pub const SEEN_INDEX_CAP: usize = 50;

/// Maximum number of active prep flags retained in the index. The TTL'd
/// `prep:<instrument>:<step>` keys remain authoritative for gate checks.
pub const PREP_INDEX_CAP: usize = 50;

/// Maximum number of active veto flags retained in the index. The TTL'd
/// `veto:<instrument>:<name>` keys remain authoritative for gate checks.
pub const VETO_INDEX_CAP: usize = 50;

/// Drop entries whose `expires_at` is at or before `now`. Used by both the
/// cooldown and seen indexes; generic over the entry type so the same pure
/// helper covers both.
pub fn prune_expired<T: HasExpiry>(entries: Vec<T>, now: DateTime<Utc>) -> Vec<T> {
    entries
        .into_iter()
        .filter(|e| e.expires_at() > now)
        .collect()
}

/// Trait for index entries that carry an expiry timestamp. Implemented for
/// [`CooldownEntry`] and [`SeenEntry`] so `prune_expired` can serve both.
pub trait HasExpiry {
    fn expires_at(&self) -> DateTime<Utc>;
}

impl HasExpiry for CooldownEntry {
    fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }
}

impl HasExpiry for SeenEntry {
    fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }
}

impl HasExpiry for PrepEntry {
    fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }
}

impl HasExpiry for VetoEntry {
    fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }
}

#[derive(Debug)]
pub enum StateError {
    Backend(String),
}

impl core::fmt::Display for StateError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Backend(msg) => write!(f, "state backend error: {msg}"),
        }
    }
}

impl std::error::Error for StateError {}

/// Cloudflare KV's minimum TTL is 60 seconds; clamp anything smaller.
pub const MIN_TTL_SECONDS: u64 = 60;

/// Split a prep KV value into its (timestamp, setter_id) parts.
///
/// The value is stored as `<rfc3339>|<setter_id>`. Values written
/// before the setter-id field was added are bare timestamps; those
/// parse with an empty setter_id, which signals "no seen-id to
/// forget" to `clear_prep` callers.
pub fn parse_prep_value(raw: &str) -> (&str, &str) {
    match raw.split_once('|') {
        Some((ts, id)) => (ts, id),
        None => (raw, ""),
    }
}

/// Clear each prep in `names` for `instrument`. Returns the subset of
/// names that were actually cleared (i.e. had a value). Used by the
/// `Prep` handler to apply the intent's `clears` list before recording
/// the new prep — supports the pattern where landing an earlier step in
/// an ordered sequence invalidates any stale later step (e.g. setting
/// `break-and-close` also drops a stale `retest`).
///
/// Errors from individual clears are returned as `Err` immediately; the
/// worker may want to log-and-continue, which it can do by mapping
/// errors at the call site rather than threading that policy through
/// here.
pub async fn clear_named_preps<S: StateStore>(
    store: &S,
    instrument: &str,
    names: &[String],
) -> Result<Vec<String>, StateError> {
    let mut cleared = Vec::new();
    for name in names {
        if let Some(setter_id) = store.clear_prep(instrument, name).await? {
            // Empty setter_id means the prep predates the wire-format
            // change that stashes the id; nothing to forget.
            if !setter_id.is_empty() {
                store.forget_seen(&setter_id).await?;
            }
            cleared.push(name.clone());
        }
    }
    Ok(cleared)
}

/// Mirror of [`clear_named_preps`] for veto names. See its docs for the
/// motivation.
pub async fn clear_named_vetos<S: StateStore>(
    store: &S,
    instrument: &str,
    names: &[String],
) -> Result<Vec<String>, StateError> {
    let mut cleared = Vec::new();
    for name in names {
        if store.clear_veto(instrument, name).await? {
            cleared.push(name.clone());
        }
    }
    Ok(cleared)
}

/// Simple in-memory [`StateStore`] used by core unit tests and by the
/// worker crate's tests. Not exposed publicly outside `cfg(test)` to
/// avoid leaking it into release builds.
#[cfg(test)]
mod memstore {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// `(value, expires_at)` for each TTL'd key.
    type Entries = HashMap<String, (String, DateTime<Utc>)>;

    #[derive(Default)]
    pub struct MemStateStore {
        inner: RefCell<Entries>,
    }

    impl MemStateStore {
        pub fn new() -> Self {
            Self::default()
        }

        fn get_live(&self, key: &str, now: DateTime<Utc>) -> Option<String> {
            let inner = self.inner.borrow();
            let (val, exp) = inner.get(key)?;
            if *exp > now { Some(val.clone()) } else { None }
        }

        fn put(&self, key: String, value: String, ttl_seconds: u64, now: DateTime<Utc>) {
            let expires_at = now + chrono::Duration::seconds(ttl_seconds as i64);
            self.inner.borrow_mut().insert(key, (value, expires_at));
        }

        fn delete(&self, key: &str) -> bool {
            self.inner.borrow_mut().remove(key).is_some()
        }
    }

    impl StateStore for MemStateStore {
        async fn is_seen(&self, id: &str) -> Result<bool, StateError> {
            Ok(self.get_live(&format!("seen:{id}"), Utc::now()).is_some())
        }
        async fn mark_seen(
            &self,
            id: &str,
            _action: Action,
            seen_at: DateTime<Utc>,
            _outcome: &str,
            ttl_seconds: u64,
        ) -> Result<(), StateError> {
            self.put(format!("seen:{id}"), "1".into(), ttl_seconds, seen_at);
            Ok(())
        }
        async fn forget_seen(&self, id: &str) -> Result<(), StateError> {
            self.delete(&format!("seen:{id}"));
            Ok(())
        }
        async fn is_cooled_down(&self, instrument: &str) -> Result<bool, StateError> {
            Ok(self
                .get_live(&format!("cooldown:{instrument}"), Utc::now())
                .is_some())
        }
        async fn set_cooldown(
            &self,
            instrument: &str,
            hours: u32,
            now: DateTime<Utc>,
        ) -> Result<(), StateError> {
            let ttl = (hours as u64).saturating_mul(3600).max(MIN_TTL_SECONDS);
            self.put(format!("cooldown:{instrument}"), "1".into(), ttl, now);
            Ok(())
        }
        async fn clear_cooldown(&self, instrument: &str) -> Result<bool, StateError> {
            Ok(self.delete(&format!("cooldown:{instrument}")))
        }
        async fn set_prep(
            &self,
            instrument: &str,
            step: &str,
            now: DateTime<Utc>,
            ttl_seconds: u64,
            setter_id: &str,
        ) -> Result<(), StateError> {
            self.put(
                format!("prep:{instrument}:{step}"),
                format!("{}|{setter_id}", now.to_rfc3339()),
                ttl_seconds.max(MIN_TTL_SECONDS),
                now,
            );
            Ok(())
        }
        async fn get_prep(
            &self,
            instrument: &str,
            step: &str,
        ) -> Result<Option<DateTime<Utc>>, StateError> {
            let Some(text) = self.get_live(&format!("prep:{instrument}:{step}"), Utc::now()) else {
                return Ok(None);
            };
            let (ts_part, _id_part) = parse_prep_value(&text);
            Ok(Some(
                DateTime::parse_from_rfc3339(ts_part)
                    .map_err(|e| StateError::Backend(format!("parse: {e}")))?
                    .with_timezone(&Utc),
            ))
        }
        async fn clear_prep(
            &self,
            instrument: &str,
            step: &str,
        ) -> Result<Option<String>, StateError> {
            let key = format!("prep:{instrument}:{step}");
            let setter = self
                .get_live(&key, Utc::now())
                .map(|raw| parse_prep_value(&raw).1.to_string());
            if self.delete(&key) {
                Ok(Some(setter.unwrap_or_default()))
            } else {
                Ok(None)
            }
        }
        async fn set_veto(
            &self,
            instrument: &str,
            name: &str,
            ttl_seconds: u64,
        ) -> Result<(), StateError> {
            self.put(
                format!("veto:{instrument}:{name}"),
                "1".into(),
                ttl_seconds.max(MIN_TTL_SECONDS),
                Utc::now(),
            );
            Ok(())
        }
        async fn is_vetoed(&self, instrument: &str, name: &str) -> Result<bool, StateError> {
            Ok(self
                .get_live(&format!("veto:{instrument}:{name}"), Utc::now())
                .is_some())
        }
        async fn clear_veto(&self, instrument: &str, name: &str) -> Result<bool, StateError> {
            Ok(self.delete(&format!("veto:{instrument}:{name}")))
        }
        async fn snapshot(&self) -> Result<Snapshot, StateError> {
            // The mock doesn't track an index alongside the TTL'd keys, so
            // the snapshot reflects whatever live keys are currently set.
            // Tests that care about the snapshot shape use the real KV
            // impl; this is for trait-contract tests of the gate logic.
            Ok(Snapshot {
                now: Utc::now(),
                cooldowns: Vec::new(),
                recent_seen: Vec::new(),
                preps: Vec::new(),
                vetos: Vec::new(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn cd(instrument: &str, expires_at: DateTime<Utc>) -> CooldownEntry {
        CooldownEntry {
            instrument: instrument.into(),
            set_at: None,
            expires_at,
        }
    }

    fn se(id: &str, expires_at: DateTime<Utc>) -> SeenEntry {
        SeenEntry {
            id: id.into(),
            action: Action::Enter,
            seen_at: None,
            outcome: String::new(),
            expires_at,
        }
    }

    #[test]
    fn prune_expired_drops_past_entries() {
        let now = ts("2026-05-14T12:00:00Z");
        let entries = vec![
            cd("EUR_USD", ts("2026-05-14T11:00:00Z")), // expired
            cd("USD_JPY", ts("2026-05-14T13:00:00Z")), // live
        ];
        let kept = prune_expired(entries, now);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].instrument, "USD_JPY");
    }

    #[test]
    fn prune_expired_drops_exactly_at_now() {
        let now = ts("2026-05-14T12:00:00Z");
        let entries = vec![se("edge", now)];
        assert!(prune_expired(entries, now).is_empty());
    }

    #[test]
    fn prune_expired_keeps_all_future() {
        let now = ts("2026-05-14T12:00:00Z");
        let entries = vec![
            se("a", ts("2026-05-14T13:00:00Z")),
            se("b", ts("2026-05-14T14:00:00Z")),
        ];
        assert_eq!(prune_expired(entries, now).len(), 2);
    }

    #[test]
    fn memstore_forget_seen_removes_record() {
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        pollster::block_on(store.mark_seen("abc", Action::Prep, Utc::now(), "ok", 3600)).unwrap();
        assert!(pollster::block_on(store.is_seen("abc")).unwrap());
        pollster::block_on(store.forget_seen("abc")).unwrap();
        assert!(!pollster::block_on(store.is_seen("abc")).unwrap());
        // Idempotent: forgetting again is a no-op.
        pollster::block_on(store.forget_seen("abc")).unwrap();
    }

    #[test]
    fn clear_named_preps_also_forgets_setter_seen_ids() {
        // The whole point of the setter-id wire format: when an
        // upstream prep (or operator `clear-prep`) drops a stale
        // downstream prep, the prep's setter message-id should be
        // dropped from `seen:` too — so the operator can re-send
        // the original prep message without hitting replay protection.
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        let now = Utc::now();

        // Two preps; each had a corresponding `mark_seen` when its
        // intent first arrived.
        pollster::block_on(store.mark_seen("retest-msg-id", Action::Prep, now, "ok", 24 * 3600))
            .unwrap();
        pollster::block_on(store.set_prep("EUR_USD", "retest", now, 3600, "retest-msg-id"))
            .unwrap();

        // Clearing the prep via clear_named_preps should also drop
        // the seen record.
        let cleared = pollster::block_on(clear_named_preps(
            &store,
            "EUR_USD",
            &["retest".to_string()],
        ))
        .unwrap();
        assert_eq!(cleared, vec!["retest".to_string()]);
        assert!(
            !pollster::block_on(store.is_seen("retest-msg-id")).unwrap(),
            "expected seen:retest-msg-id to be forgotten after clear_named_preps"
        );
    }

    #[test]
    fn legacy_prep_value_without_setter_id_parses_clean() {
        // Old prep values (pre-setter-id) are bare RFC3339 strings.
        // The parser must still return them so `get_prep` keeps
        // working after a deploy that includes the new format —
        // existing live preps don't suddenly become invalid.
        let (ts, id) = parse_prep_value("2026-05-19T10:00:00+00:00");
        assert_eq!(ts, "2026-05-19T10:00:00+00:00");
        assert_eq!(id, "");

        let (ts, id) = parse_prep_value("2026-05-19T10:00:00+00:00|some-id");
        assert_eq!(ts, "2026-05-19T10:00:00+00:00");
        assert_eq!(id, "some-id");
    }

    #[test]
    fn memstore_prep_round_trip() {
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        // Use `Utc::now()` so the memstore's wall-clock liveness check
        // sees the entry as live. The test only cares about round-trip
        // semantics, not TTL expiry.
        let now = Utc::now();
        pollster::block_on(store.set_prep("EUR_USD", "break", now, 4 * 3600, "setter-1")).unwrap();
        let got = pollster::block_on(store.get_prep("EUR_USD", "break")).unwrap();
        assert_eq!(got, Some(now));
        let cleared = pollster::block_on(store.clear_prep("EUR_USD", "break")).unwrap();
        assert_eq!(cleared.as_deref(), Some("setter-1"));
        let got = pollster::block_on(store.get_prep("EUR_USD", "break")).unwrap();
        assert!(got.is_none());
        // Clearing again returns None — the prep is gone.
        let again = pollster::block_on(store.clear_prep("EUR_USD", "break")).unwrap();
        assert!(again.is_none());
    }

    #[test]
    fn memstore_get_prep_absent() {
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        let got = pollster::block_on(store.get_prep("EUR_USD", "ghost")).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn memstore_veto_round_trip() {
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        pollster::block_on(store.set_veto("EUR_USD", "news-window", 6 * 3600)).unwrap();
        assert!(pollster::block_on(store.is_vetoed("EUR_USD", "news-window")).unwrap());
        let was = pollster::block_on(store.clear_veto("EUR_USD", "news-window")).unwrap();
        assert!(was);
        assert!(!pollster::block_on(store.is_vetoed("EUR_USD", "news-window")).unwrap());
    }

    #[test]
    fn memstore_preps_per_instrument_are_independent() {
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        let t1 = Utc::now();
        let t2 = t1 + chrono::Duration::minutes(5);
        pollster::block_on(store.set_prep("EUR_USD", "break", t1, 3600, "id-1")).unwrap();
        pollster::block_on(store.set_prep("USD_JPY", "break", t2, 3600, "id-2")).unwrap();
        assert_eq!(
            pollster::block_on(store.get_prep("EUR_USD", "break")).unwrap(),
            Some(t1)
        );
        assert_eq!(
            pollster::block_on(store.get_prep("USD_JPY", "break")).unwrap(),
            Some(t2)
        );
    }

    #[test]
    fn memstore_set_prep_overwrites_timestamp() {
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        let t1 = Utc::now();
        let t2 = t1 + chrono::Duration::hours(1);
        // Use a TTL that comfortably covers the test's relative clock —
        // memstore's `get_live` consults the real wall clock.
        let ttl = 24 * 3600;
        pollster::block_on(store.set_prep("EUR_USD", "break", t1, ttl, "id-a")).unwrap();
        pollster::block_on(store.set_prep("EUR_USD", "break", t2, ttl, "id-b")).unwrap();
        // Refiring a prep refreshes its timestamp — documented behaviour.
        assert_eq!(
            pollster::block_on(store.get_prep("EUR_USD", "break")).unwrap(),
            Some(t2)
        );
    }

    #[test]
    fn clear_named_preps_removes_only_listed_names() {
        // The core of the prep-ordering bug fix: when a fresh
        // `break-and-close` lands, any stale `retest` from before it
        // must be wiped so a future `requires_preps: [break-and-close,
        // retest]` gate doesn't satisfy on the stale retest.
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        let now = Utc::now();
        let ttl = 24 * 3600;
        pollster::block_on(store.set_prep("EUR_USD", "retest", now, ttl, "retest-id")).unwrap();
        pollster::block_on(store.set_prep("EUR_USD", "other", now, ttl, "other-id")).unwrap();

        let cleared = pollster::block_on(clear_named_preps(
            &store,
            "EUR_USD",
            &["retest".to_string(), "ghost".to_string()],
        ))
        .unwrap();
        // `retest` was present; `ghost` was not. Only the present one
        // is reported in the cleared set.
        assert_eq!(cleared, vec!["retest".to_string()]);

        // Untargeted prep survives.
        assert!(
            pollster::block_on(store.get_prep("EUR_USD", "other"))
                .unwrap()
                .is_some()
        );
        // Targeted prep is gone.
        assert!(
            pollster::block_on(store.get_prep("EUR_USD", "retest"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn clear_named_preps_on_empty_list_is_a_noop() {
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        let now = Utc::now();
        pollster::block_on(store.set_prep("EUR_USD", "retest", now, 24 * 3600, "retest-id"))
            .unwrap();
        let cleared = pollster::block_on(clear_named_preps(&store, "EUR_USD", &[])).unwrap();
        assert!(cleared.is_empty());
        // Existing prep untouched.
        assert!(
            pollster::block_on(store.get_prep("EUR_USD", "retest"))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn clear_named_preps_scope_is_per_instrument() {
        // Clearing on EUR_USD must not touch USD_JPY's prep of the same
        // name.
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        let now = Utc::now();
        let ttl = 24 * 3600;
        pollster::block_on(store.set_prep("EUR_USD", "retest", now, ttl, "eur-id")).unwrap();
        pollster::block_on(store.set_prep("USD_JPY", "retest", now, ttl, "jpy-id")).unwrap();

        let cleared = pollster::block_on(clear_named_preps(
            &store,
            "EUR_USD",
            &["retest".to_string()],
        ))
        .unwrap();
        assert_eq!(cleared, vec!["retest".to_string()]);
        assert!(
            pollster::block_on(store.get_prep("EUR_USD", "retest"))
                .unwrap()
                .is_none()
        );
        // USD_JPY untouched.
        assert!(
            pollster::block_on(store.get_prep("USD_JPY", "retest"))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn clear_named_vetos_removes_only_listed_names() {
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        let ttl = 24 * 3600;
        pollster::block_on(store.set_veto("EUR_USD", "news", ttl)).unwrap();
        pollster::block_on(store.set_veto("EUR_USD", "other", ttl)).unwrap();

        let cleared =
            pollster::block_on(clear_named_vetos(&store, "EUR_USD", &["news".to_string()]))
                .unwrap();
        assert_eq!(cleared, vec!["news".to_string()]);
        assert!(!pollster::block_on(store.is_vetoed("EUR_USD", "news")).unwrap());
        assert!(pollster::block_on(store.is_vetoed("EUR_USD", "other")).unwrap());
    }

    #[test]
    fn prune_expired_works_on_prep_entries() {
        let now = ts("2026-05-14T12:00:00Z");
        let entries = vec![
            PrepEntry {
                instrument: "EUR_USD".into(),
                step: "stale".into(),
                set_at: ts("2026-05-14T10:00:00Z"),
                expires_at: ts("2026-05-14T11:00:00Z"), // expired
            },
            PrepEntry {
                instrument: "EUR_USD".into(),
                step: "fresh".into(),
                set_at: ts("2026-05-14T11:30:00Z"),
                expires_at: ts("2026-05-14T15:00:00Z"), // live
            },
        ];
        let kept = prune_expired(entries, now);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].step, "fresh");
    }

    #[test]
    fn prune_expired_works_on_veto_entries() {
        let now = ts("2026-05-14T12:00:00Z");
        let entries = vec![
            VetoEntry {
                instrument: "EUR_USD".into(),
                name: "stale".into(),
                expires_at: ts("2026-05-14T11:00:00Z"),
            },
            VetoEntry {
                instrument: "USD_JPY".into(),
                name: "fresh".into(),
                expires_at: ts("2026-05-14T13:00:00Z"),
            },
        ];
        let kept = prune_expired(entries, now);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].name, "fresh");
    }

    #[test]
    fn snapshot_serialises_new_sections_as_yaml() {
        // The status action serialises Snapshot as YAML; verify the new
        // sections come through cleanly when populated.
        let snap = Snapshot {
            now: ts("2026-05-14T12:00:00Z"),
            cooldowns: Vec::new(),
            recent_seen: Vec::new(),
            preps: vec![PrepEntry {
                instrument: "EUR_USD".into(),
                step: "break-and-close".into(),
                set_at: ts("2026-05-14T11:00:00Z"),
                expires_at: ts("2026-05-14T15:00:00Z"),
            }],
            vetos: vec![VetoEntry {
                instrument: "EUR_USD".into(),
                name: "news-window".into(),
                expires_at: ts("2026-05-14T13:00:00Z"),
            }],
        };
        let yaml = serde_yaml::to_string(&snap).unwrap();
        assert!(yaml.contains("preps:"));
        assert!(yaml.contains("step: break-and-close"));
        assert!(yaml.contains("vetos:"));
        assert!(yaml.contains("name: news-window"));
    }

    #[test]
    fn snapshot_deserialises_without_new_sections_for_back_compat() {
        // Pre-existing serialised snapshots (e.g. in unit tests, or any
        // stored copies) have no `preps:` / `vetos:` fields. Make sure
        // they still parse — the new fields default to empty.
        let yaml = "now: \"2026-05-14T12:00:00Z\"\ncooldowns: []\nrecent_seen: []\n";
        let snap: Snapshot = serde_yaml::from_str(yaml).unwrap();
        assert!(snap.preps.is_empty());
        assert!(snap.vetos.is_empty());
    }

    #[test]
    fn prune_expired_drops_all_past() {
        let now = ts("2026-05-14T12:00:00Z");
        let entries = vec![
            cd("A", ts("2026-05-13T12:00:00Z")),
            cd("B", ts("2026-05-13T11:00:00Z")),
        ];
        assert!(prune_expired(entries, now).is_empty());
    }

    #[test]
    fn seen_entry_round_trips_legacy_yaml() {
        // Older entries in live KV may not have action/seen_at/outcome.
        // They must still deserialise via the serde defaults.
        let yaml = "id: legacy\nexpires_at: \"2026-05-14T13:00:00Z\"\n";
        let entry: SeenEntry = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(entry.id, "legacy");
        assert_eq!(entry.action, Action::Enter);
        assert_eq!(entry.seen_at, None);
        assert!(entry.outcome.is_empty());
    }

    #[test]
    fn cooldown_entry_round_trips_legacy_yaml() {
        let yaml = "instrument: EUR_USD\nexpires_at: \"2026-05-14T13:00:00Z\"\n";
        let entry: CooldownEntry = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(entry.instrument, "EUR_USD");
        assert_eq!(entry.set_at, None);
    }

    #[test]
    fn seen_entry_serialises_with_action_seen_at_outcome() {
        // The `status` snapshot is the primary consumer. Confirm the YAML
        // shape is exactly what an operator sees.
        let entry = SeenEntry {
            id: "F40-2026-05-15-729f".into(),
            action: Action::Enter,
            seen_at: Some(ts("2026-05-15T18:00:00Z")),
            outcome: "rejected: cooled-down".into(),
            expires_at: ts("2026-05-16T03:33:01Z"),
        };
        let yaml = serde_yaml::to_string(&entry).unwrap();
        // Round-trip through serde to assert on the parsed shape rather
        // than YAML formatting quirks (timestamp quoting, etc).
        let parsed: SeenEntry = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.action, Action::Enter);
        assert_eq!(parsed.outcome, "rejected: cooled-down");
        assert_eq!(parsed.seen_at, Some(ts("2026-05-15T18:00:00Z")));
        assert_eq!(parsed.expires_at, ts("2026-05-16T03:33:01Z"));
    }

    #[test]
    fn cooldown_entry_serialises_with_set_at() {
        let entry = CooldownEntry {
            instrument: "F40".into(),
            set_at: Some(ts("2026-05-15T18:00:34Z")),
            expires_at: ts("2026-05-16T06:00:34Z"),
        };
        let yaml = serde_yaml::to_string(&entry).unwrap();
        let parsed: CooldownEntry = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.instrument, "F40");
        assert_eq!(parsed.set_at, Some(ts("2026-05-15T18:00:34Z")));
        assert_eq!(parsed.expires_at, ts("2026-05-16T06:00:34Z"));
    }
}
