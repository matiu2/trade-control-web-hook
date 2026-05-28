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

use crate::intent::{Action, Direction};

/// One active cooldown row in a [`Snapshot`]. `set_at` records when the
/// cooldown was put in place so the operator can see how long ago it
/// started; `expires_at` is when it lapses on its own.
///
/// `account` scopes the cooldown the same way [`VetoEntry`] does:
/// `None` = worker-global (pauses every account), `Some(name)` =
/// account-scoped. See [`ACCOUNT_SCOPE_GLOBAL`] for the on-disk
/// sentinel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CooldownEntry {
    pub instrument: String,
    /// Backfilled to `expires_at - hours` when missing (older entries in
    /// live KV predate this field).
    #[serde(default)]
    pub set_at: Option<DateTime<Utc>>,
    pub expires_at: DateTime<Utc>,
    /// `None` for worker-global, `Some(name)` for account-scoped. No
    /// `serde(default)` — pre-scoping cooldowns in KV are wiped at
    /// deploy time, same as vetos.
    pub account: Option<String>,
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
    /// Trade grouping id stamped on the alert, when present. Lets
    /// `status` filter by trade and (later) bulk-cancel by trade. None
    /// for legacy entries and for alerts that opted out of grouping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trade_id: Option<String>,
}

fn default_action() -> Action {
    Action::Enter
}

/// One active "prep" flag row in a [`Snapshot`]. A prep records that a
/// named step (e.g. `break-and-close`) landed for an instrument at a
/// specific time; the `enter` gate checks both presence and order.
///
/// `account` scopes the prep — a setup in progress on one account is
/// not the same as a setup on another account, even on the same pair.
/// Same scoping rules as [`VetoEntry`] / [`CooldownEntry`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrepEntry {
    pub instrument: String,
    pub step: String,
    pub set_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    /// `None` for worker-global, `Some(name)` for account-scoped. No
    /// `serde(default)` — pre-scoping preps in KV are wiped at deploy
    /// time, same as vetos and cooldowns.
    pub account: Option<String>,
}

/// One active "veto" flag row in a [`Snapshot`]. Presence alone is the
/// signal — no timestamp ordering applies on vetos.
///
/// `account` scopes the veto to a single configured account name.
/// `None` means the veto is worker-global — it applies to any account
/// (or to a single-account worker that never names accounts in its
/// intents). See [`ACCOUNT_SCOPE_GLOBAL`] for the on-disk sentinel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VetoEntry {
    pub instrument: String,
    pub name: String,
    pub expires_at: DateTime<Utc>,
    /// `None` for worker-global, `Some(name)` for account-scoped.
    /// Pre-scoping entries in KV (written before this field existed)
    /// are wiped at deploy time — there is no on-disk back-compat.
    pub account: Option<String>,
}

/// One placed entry attempt for a `(account, trade_id)` group, written
/// after `place_order` succeeds when `intent.max_retries.is_some()`.
/// The worker uses the list of these rows to count attempts against
/// the cap and to cross-reference broker state for each prior attempt
/// (still pending, filled and open, filled and closed, vanished).
///
/// `broker_trade_id` is snapshotted lazily: the first lookup that
/// finds this attempt as an open trade writes the upstream
/// `BrokerTrade.id` back onto the row so subsequent closed-trade
/// lookups can correlate (`ClosedTrade` carries no
/// `originating_order_id` field on either broker).
///
/// On OANDA `broker_trade_id` happens to equal `broker_order_id`; on
/// TradeNation the trade id is the distinct PositionID. Don't assume
/// identity in callers — always correlate via the stored field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntryAttempt {
    pub trade_id: String,
    /// `None` for worker-global, `Some(name)` for account-scoped. Same
    /// shape as [`VetoEntry`] / [`PrepEntry`] / [`CooldownEntry`].
    pub account: Option<String>,
    pub instrument: String,
    /// 1-based: the first attempt is `1`, the Nth is `N`.
    pub attempt_no: u32,
    /// What `place_order` returned.
    pub broker_order_id: String,
    /// Snapshotted the first time the worker observes this attempt
    /// has filled into an open trade. `None` until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broker_trade_id: Option<String>,
    pub direction: Direction,
    pub placed_at: DateTime<Utc>,
    /// The firing bar's `shell.time`. Used together with
    /// `(account, trade_id)` to dedup multi-fire arrivals within
    /// one bar.
    pub shell_time: DateTime<Utc>,
    /// When this row stops mattering — typically `intent.not_after`
    /// plus a grace period so the lookup window outlives the
    /// alert window itself.
    pub expires_at: DateTime<Utc>,
    /// Resolved absolute stop-loss price at placement time. Used by the
    /// scheduled SL-breach sweep to decide whether a still-pending order
    /// has been overtaken by price. `None` on rows written before this
    /// field existed — the sweep treats `None` as "skip" so legacy rows
    /// expire normally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_loss_price: Option<f64>,
}

impl HasExpiry for EntryAttempt {
    fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }
}

/// One active "pause" row. A pause suspends `enter` actions for the
/// parent `trade_id` until a matching
/// [`Action::Resume`][crate::intent::Action::Resume] alert clears it.
///
/// Unlike veto/prep/cooldown the key is scoped by `trade_id` +
/// `blackout_id` rather than `(account, instrument)` — `trade_id` is
/// globally unique and a pause inherently targets one specific setup,
/// not "every entry on this pair". Multiple concurrent blackouts on a
/// single trade are allowed: `pause:<trade_id>:nfp` and
/// `pause:<trade_id>:cb-rate-decision` coexist, each cleared by its
/// own resume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PauseEntry {
    pub trade_id: String,
    pub blackout_id: String,
    /// Free-form human label (e.g. `"news:USD-NFP-2026-06-06"`)
    /// surfaced on the seen-index outcome string so operators can
    /// answer "why is this trade paused?" without inspecting chart
    /// drawings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub set_at: DateTime<Utc>,
    /// Pauses don't auto-expire on a TTL — the matching `resume`
    /// alert is the authoritative clear. We apply a long safety TTL
    /// (driven by the alert's `not_after` window plus grace) so an
    /// orphaned pause from a dropped `resume` eventually ages out
    /// instead of pinning the trade forever.
    pub expires_at: DateTime<Utc>,
}

impl HasExpiry for PauseEntry {
    fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }
}

/// One active "news window" row. Keyed by `trade_id` + `news_id`, this
/// records that a known volatility window (2-star+ scheduled news on
/// a currency the parent trade is exposed to) is currently open.
///
/// Independent of [`PauseEntry`]: a news window does **not** by itself
/// block new entries. It exists so that *another* alert — a
/// golden-reversal candle on the opposite-direction side of the same
/// Pine study — can flatten the open trade only inside the window,
/// while the same reversal candle outside the window is ignored. The
/// gate lives on `Close` intents that carry `require_news_window:
/// true`; see `src/lib.rs` for the read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewsEntry {
    pub trade_id: String,
    pub news_id: String,
    /// Free-form human label (e.g. `"USD-NFP-2026-06-06"`) surfaced on
    /// the seen-index outcome string so operators can answer "why did
    /// this trade flatten?" weeks later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub set_at: DateTime<Utc>,
    /// News windows don't auto-expire on a TTL — the matching
    /// `news-end` alert is the authoritative clear. The safety TTL
    /// (driven by the alert's `not_after` plus grace) just stops an
    /// orphaned window pinning the trade forever.
    pub expires_at: DateTime<Utc>,
}

impl HasExpiry for NewsEntry {
    fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }
}

/// KV-key sentinel used in place of an account name when a veto is
/// worker-global (no `account:` field on the intent). Picked because
/// account names use kebab-case and never appear as a bare underscore,
/// so `veto:_:EUR_USD:news` is unambiguously the global key.
pub const ACCOUNT_SCOPE_GLOBAL: &str = "_";

/// Resolve an optional account name into the string segment used in KV
/// keys. `None` → [`ACCOUNT_SCOPE_GLOBAL`]; `Some(name)` → the name
/// unchanged. Centralised so the encoding lives in one place.
pub fn account_scope(account: Option<&str>) -> &str {
    account.unwrap_or(ACCOUNT_SCOPE_GLOBAL)
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
    /// Active blackout pauses across all trades. `default`-empty for
    /// back-compat with snapshots serialised before the field landed.
    #[serde(default)]
    pub pauses: Vec<PauseEntry>,
    /// Active news windows across all trades. Same shape as
    /// [`Self::pauses`] but a separate namespace — news windows
    /// don't block entries; they gate reversal-candle closes. See
    /// [`NewsEntry`].
    #[serde(default)]
    pub news_windows: Vec<NewsEntry>,
}

/// Async storage interface. Implementations are `?Send` because the CF Worker
/// runtime is single-threaded WASM and its KV handle is `!Send`.
pub trait StateStore {
    /// Returns true if `id` has already been recorded as seen.
    fn is_seen(&self, id: &str) -> impl Future<Output = Result<bool, StateError>>;

    /// Mark `id` as seen with a TTL in seconds, recording the action and
    /// outcome that ran on this id so `status` can show what happened.
    /// `trade_id` is the optional grouping correlator stamped on the
    /// incoming alert; stashed in the seen-index entry for later
    /// status-filter / bulk-cancel work.
    fn mark_seen(
        &self,
        id: &str,
        action: Action,
        seen_at: DateTime<Utc>,
        outcome: &str,
        ttl_seconds: u64,
        trade_id: Option<&str>,
    ) -> impl Future<Output = Result<(), StateError>>;

    /// Delete the `seen:<id>` replay-protection record and prune the
    /// index. Used when a prep is cleared so the operator can re-send
    /// the original prep message without hitting the duplicate-id 409.
    /// Best-effort: succeeds even if the key is already gone.
    fn forget_seen(&self, id: &str) -> impl Future<Output = Result<(), StateError>>;

    /// Returns true if `instrument` is currently under cooldown for
    /// `account`. Same global-first semantics as [`Self::is_vetoed`]: a
    /// global cooldown (no `account:` on the setter intent) pauses
    /// every account, an account-scoped cooldown pauses only that
    /// account.
    fn is_cooled_down(
        &self,
        account: Option<&str>,
        instrument: &str,
    ) -> impl Future<Output = Result<bool, StateError>>;

    /// Set a cooldown on `(account, instrument)` for `hours`. `now` is
    /// recorded as the cooldown's start time so `status` shows how
    /// long ago it began.
    fn set_cooldown(
        &self,
        account: Option<&str>,
        instrument: &str,
        hours: u32,
        now: DateTime<Utc>,
    ) -> impl Future<Output = Result<(), StateError>>;

    /// Clear the cooldown for `(account, instrument)`. Returns whether
    /// it was set before. Scoped — clearing on one account doesn't
    /// touch a different account's cooldown or the global cooldown.
    fn clear_cooldown(
        &self,
        account: Option<&str>,
        instrument: &str,
    ) -> impl Future<Output = Result<bool, StateError>>;

    /// Record a named prep step for `(account, instrument)` with a TTL.
    /// `now` is the timestamp stored on the flag; the entry-time gate
    /// uses it to enforce ordering across multiple preps. `setter_id`
    /// is the message-id that set this prep, stashed inside the value
    /// so `clear_prep` can also forget that id's `seen:<id>` record —
    /// the operator can then re-send the original prep message
    /// without hitting the replay-protection 409.
    fn set_prep(
        &self,
        account: Option<&str>,
        instrument: &str,
        step: &str,
        now: DateTime<Utc>,
        ttl_seconds: u64,
        setter_id: &str,
    ) -> impl Future<Output = Result<(), StateError>>;

    /// Return `Some(set_at)` if the prep is currently active for
    /// `(account, instrument)`, `None` otherwise (absent or expired).
    /// Global-first lookup, same shape as [`Self::is_vetoed`].
    fn get_prep(
        &self,
        account: Option<&str>,
        instrument: &str,
        step: &str,
    ) -> impl Future<Output = Result<Option<DateTime<Utc>>, StateError>>;

    /// Clear a prep flag for `(account, instrument)`. Returns
    /// `Some(setter_id)` if the prep was active and recorded a setter
    /// id, `Some(String::new())` if it was active but predates the
    /// setter-id wire format, and `None` if it wasn't set. Scoped to
    /// the supplied account.
    fn clear_prep(
        &self,
        account: Option<&str>,
        instrument: &str,
        step: &str,
    ) -> impl Future<Output = Result<Option<String>, StateError>>;

    /// Record a named veto for `instrument` with a TTL. Presence alone
    /// is the signal — no timestamp needs storing.
    ///
    /// `account` scopes the veto: `None` means worker-global (any
    /// account on this instrument), `Some(name)` means only that
    /// account is affected. Two accounts trading the same pair therefore
    /// don't shadow each other's vetos.
    fn set_veto(
        &self,
        account: Option<&str>,
        instrument: &str,
        name: &str,
        ttl_seconds: u64,
    ) -> impl Future<Output = Result<(), StateError>>;

    /// Returns true if the veto is currently active for `account` on
    /// `instrument`. A `Some(name)` query matches a `None` (global)
    /// veto as well as a matching `Some(name)` veto — global vetos
    /// affect every account by design.
    fn is_vetoed(
        &self,
        account: Option<&str>,
        instrument: &str,
        name: &str,
    ) -> impl Future<Output = Result<bool, StateError>>;

    /// Clear a veto flag for `account` on `instrument`. Returns whether
    /// it was set before. Clearing is scoped — clearing on one account
    /// doesn't drop a different account's veto or the global veto.
    fn clear_veto(
        &self,
        account: Option<&str>,
        instrument: &str,
        name: &str,
    ) -> impl Future<Output = Result<bool, StateError>>;

    /// Set a blackout pause for `(trade_id, blackout_id)`. The
    /// `enter` gate rejects while any pause for `trade_id` is active.
    /// `ttl_seconds` is a safety net — the matching `resume` is the
    /// authoritative clear. Refiring (same trade_id + blackout_id)
    /// overwrites the prior entry, refreshing the TTL.
    fn set_pause(
        &self,
        trade_id: &str,
        blackout_id: &str,
        reason: Option<&str>,
        now: DateTime<Utc>,
        ttl_seconds: u64,
    ) -> impl Future<Output = Result<(), StateError>>;

    /// Return all currently-active pauses for `trade_id`, in
    /// unspecified order. The list is non-empty iff at least one
    /// blackout window is active. The `enter` gate calls this and
    /// rejects on any hit.
    fn list_pauses_for_trade(
        &self,
        trade_id: &str,
    ) -> impl Future<Output = Result<Vec<PauseEntry>, StateError>>;

    /// Clear a single `(trade_id, blackout_id)` pause. Returns whether
    /// it was set before. Siblings (different `blackout_id` on the
    /// same trade) are untouched.
    fn clear_pause(
        &self,
        trade_id: &str,
        blackout_id: &str,
    ) -> impl Future<Output = Result<bool, StateError>>;

    /// Open a news window for `(trade_id, news_id)`. While at least one
    /// window for a trade is open, a `Close` intent with
    /// `require_news_window: true` is allowed to flatten the position;
    /// outside any window, the same intent is rejected. Re-firing
    /// (same trade_id + news_id) overwrites the prior entry and
    /// refreshes the TTL. `ttl_seconds` is a safety net — the matching
    /// `news-end` is the authoritative clear.
    fn set_news_window(
        &self,
        trade_id: &str,
        news_id: &str,
        reason: Option<&str>,
        now: DateTime<Utc>,
        ttl_seconds: u64,
    ) -> impl Future<Output = Result<(), StateError>>;

    /// Return all currently-open news windows for `trade_id`. The
    /// reversal-close gate calls this and accepts the close iff the
    /// list is non-empty.
    fn list_news_windows_for_trade(
        &self,
        trade_id: &str,
    ) -> impl Future<Output = Result<Vec<NewsEntry>, StateError>>;

    /// Close a single `(trade_id, news_id)` window. Returns whether
    /// it was open before. Sibling windows on the same trade are
    /// untouched.
    fn clear_news_window(
        &self,
        trade_id: &str,
        news_id: &str,
    ) -> impl Future<Output = Result<bool, StateError>>;

    /// Return a snapshot of active cooldowns and recent seen ids.
    fn snapshot(&self) -> impl Future<Output = Result<Snapshot, StateError>>;

    /// Record a placed entry attempt for a `(account, trade_id)`
    /// group. Used by the retry gate after `place_order` succeeds.
    fn record_entry_attempt(
        &self,
        attempt: EntryAttempt,
    ) -> impl Future<Output = Result<(), StateError>>;

    /// Return all entry attempts for `(account, trade_id)`, ordered
    /// by `attempt_no` ascending. Used by the retry gate to count
    /// prior attempts and cross-reference each against broker state.
    fn list_entry_attempts(
        &self,
        account: Option<&str>,
        trade_id: &str,
    ) -> impl Future<Output = Result<Vec<EntryAttempt>, StateError>>;

    /// Return every still-tracked entry attempt across all
    /// `(account, trade_id)` groups, in unspecified order. Used by
    /// the scheduled sweep to walk pending orders for SL-breach and
    /// expiry checks.
    fn list_all_entry_attempts(
        &self,
    ) -> impl Future<Output = Result<Vec<EntryAttempt>, StateError>>;

    /// Delete a single `EntryAttempt` row by `(account, trade_id,
    /// attempt_no)`. Used by the scheduled sweep after a successful
    /// cancel. Best-effort: succeeds even if the row is already gone.
    fn delete_entry_attempt(
        &self,
        account: Option<&str>,
        trade_id: &str,
        attempt_no: u32,
    ) -> impl Future<Output = Result<(), StateError>>;

    /// Update a previously-recorded attempt with the broker's trade
    /// id, snapshotted lazily the first time the worker observes the
    /// attempt has filled into an open trade. Idempotent — calling
    /// twice with the same value is fine.
    fn set_entry_attempt_broker_trade_id(
        &self,
        account: Option<&str>,
        trade_id: &str,
        attempt_no: u32,
        broker_trade_id: &str,
    ) -> impl Future<Output = Result<(), StateError>>;

    /// Returns true if this `(account, trade_id, shell_time)` retry
    /// fire has already been seen. Replaces the `seen:<id>` dedup for
    /// the multi-shot path so two arrivals on the same firing bar
    /// don't double-place.
    fn is_retry_fire_seen(
        &self,
        account: Option<&str>,
        trade_id: &str,
        shell_time: DateTime<Utc>,
    ) -> impl Future<Output = Result<bool, StateError>>;

    /// Record a retry fire as seen. TTL is in seconds.
    fn mark_retry_fire_seen(
        &self,
        account: Option<&str>,
        trade_id: &str,
        shell_time: DateTime<Utc>,
        ttl_seconds: u64,
    ) -> impl Future<Output = Result<(), StateError>>;
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

/// Compute the effective TTL (in seconds) for a veto record.
///
/// **Motivating example.** A setup is sent with a `not_after` 4 days
/// out (the alert is valid for that long). Mid-window, price runs
/// past the entry zone — TradingView fires a `veto: too-high` with
/// `ttl_hours: 12`. The naive interpretation would expire that veto
/// 12h later, *while the original setup is still alive*, leaving the
/// stale entry unguarded. But "price went too high" invalidates the
/// setup for as long as that setup itself is valid — what we actually
/// want is "this veto kills this setup, full stop."
///
/// So the rule is: the veto lives until the latest of
///   1. `now + ttl_hours` (the bare cooldown after the veto fires), and
///   2. `not_after + ttl_hours` (the alert's own expiry, plus a tail
///      so the veto doesn't lapse the instant the setup does and let
///      a retry sneak in on a clock skew).
///
/// Equivalent closed form: `(max(now, not_after) - now) + ttl`.
/// When `not_after` is already in the past the second clause adds
/// nothing and we fall back to the bare TTL.
///
/// Note this binds the veto to **this** alert window only — a later,
/// independent setup at a different price gets its own `not_after`
/// and isn't affected. The veto isn't "forever," it's "for the life
/// of the thing it vetoed."
///
/// `not_after - now` is clamped to zero so a past `not_after` doesn't
/// shorten the TTL. Output is clamped to [`MIN_TTL_SECONDS`].
pub fn veto_ttl_seconds(ttl_hours: u32, not_after: DateTime<Utc>, now: DateTime<Utc>) -> u64 {
    let base = (ttl_hours as u64).saturating_mul(3600);
    let remaining = (not_after - now).num_seconds().max(0) as u64;
    base.saturating_add(remaining).max(MIN_TTL_SECONDS)
}

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
    account: Option<&str>,
    instrument: &str,
    names: &[String],
) -> Result<Vec<String>, StateError> {
    let mut cleared = Vec::new();
    for name in names {
        if let Some(setter_id) = store.clear_prep(account, instrument, name).await? {
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
///
/// `account` scopes the clear: clearing a veto on `Some("acct-a")`
/// doesn't drop the same-named veto on `Some("acct-b")` or the global
/// (`None`) veto. The caller decides the scope.
pub async fn clear_named_vetos<S: StateStore>(
    store: &S,
    account: Option<&str>,
    instrument: &str,
    names: &[String],
) -> Result<Vec<String>, StateError> {
    let mut cleared = Vec::new();
    for name in names {
        if store.clear_veto(account, instrument, name).await? {
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

    /// Attempts keyed by `(account_scope, trade_id)` → ordered list.
    type AttemptMap = HashMap<(String, String), Vec<EntryAttempt>>;

    #[derive(Default)]
    pub struct MemStateStore {
        inner: RefCell<Entries>,
        attempts: RefCell<AttemptMap>,
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
            _trade_id: Option<&str>,
        ) -> Result<(), StateError> {
            self.put(format!("seen:{id}"), "1".into(), ttl_seconds, seen_at);
            Ok(())
        }
        async fn forget_seen(&self, id: &str) -> Result<(), StateError> {
            self.delete(&format!("seen:{id}"));
            Ok(())
        }
        async fn is_cooled_down(
            &self,
            account: Option<&str>,
            instrument: &str,
        ) -> Result<bool, StateError> {
            let now = Utc::now();
            // Global cooldowns pause every account; check both keys.
            let global = format!("cooldown:{ACCOUNT_SCOPE_GLOBAL}:{instrument}");
            if self.get_live(&global, now).is_some() {
                return Ok(true);
            }
            if let Some(name_str) = account {
                let scoped = format!("cooldown:{name_str}:{instrument}");
                if self.get_live(&scoped, now).is_some() {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        async fn set_cooldown(
            &self,
            account: Option<&str>,
            instrument: &str,
            hours: u32,
            now: DateTime<Utc>,
        ) -> Result<(), StateError> {
            let scope = account_scope(account);
            let ttl = (hours as u64).saturating_mul(3600).max(MIN_TTL_SECONDS);
            self.put(
                format!("cooldown:{scope}:{instrument}"),
                "1".into(),
                ttl,
                now,
            );
            Ok(())
        }
        async fn clear_cooldown(
            &self,
            account: Option<&str>,
            instrument: &str,
        ) -> Result<bool, StateError> {
            let scope = account_scope(account);
            Ok(self.delete(&format!("cooldown:{scope}:{instrument}")))
        }
        async fn set_prep(
            &self,
            account: Option<&str>,
            instrument: &str,
            step: &str,
            now: DateTime<Utc>,
            ttl_seconds: u64,
            setter_id: &str,
        ) -> Result<(), StateError> {
            let scope = account_scope(account);
            self.put(
                format!("prep:{scope}:{instrument}:{step}"),
                format!("{}|{setter_id}", now.to_rfc3339()),
                ttl_seconds.max(MIN_TTL_SECONDS),
                now,
            );
            Ok(())
        }
        async fn get_prep(
            &self,
            account: Option<&str>,
            instrument: &str,
            step: &str,
        ) -> Result<Option<DateTime<Utc>>, StateError> {
            let now = Utc::now();
            // Global preps satisfy the gate on every account; check
            // global first, then scoped if present.
            let global = format!("prep:{ACCOUNT_SCOPE_GLOBAL}:{instrument}:{step}");
            let text = self.get_live(&global, now).or_else(|| {
                account
                    .map(|n| format!("prep:{n}:{instrument}:{step}"))
                    .and_then(|k| self.get_live(&k, now))
            });
            let Some(text) = text else {
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
            account: Option<&str>,
            instrument: &str,
            step: &str,
        ) -> Result<Option<String>, StateError> {
            let scope = account_scope(account);
            let key = format!("prep:{scope}:{instrument}:{step}");
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
            account: Option<&str>,
            instrument: &str,
            name: &str,
            ttl_seconds: u64,
        ) -> Result<(), StateError> {
            let scope = account_scope(account);
            self.put(
                format!("veto:{scope}:{instrument}:{name}"),
                "1".into(),
                ttl_seconds.max(MIN_TTL_SECONDS),
                Utc::now(),
            );
            Ok(())
        }
        async fn is_vetoed(
            &self,
            account: Option<&str>,
            instrument: &str,
            name: &str,
        ) -> Result<bool, StateError> {
            let now = Utc::now();
            // Global vetos cover every account; check both keys.
            let global = format!("veto:{ACCOUNT_SCOPE_GLOBAL}:{instrument}:{name}");
            if self.get_live(&global, now).is_some() {
                return Ok(true);
            }
            if let Some(name_str) = account {
                let scoped = format!("veto:{name_str}:{instrument}:{name}");
                if self.get_live(&scoped, now).is_some() {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        async fn clear_veto(
            &self,
            account: Option<&str>,
            instrument: &str,
            name: &str,
        ) -> Result<bool, StateError> {
            let scope = account_scope(account);
            Ok(self.delete(&format!("veto:{scope}:{instrument}:{name}")))
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
                pauses: Vec::new(),
                news_windows: Vec::new(),
            })
        }

        async fn set_pause(
            &self,
            trade_id: &str,
            blackout_id: &str,
            reason: Option<&str>,
            now: DateTime<Utc>,
            ttl_seconds: u64,
        ) -> Result<(), StateError> {
            let key = format!("pause:{trade_id}:{blackout_id}");
            let entry = PauseEntry {
                trade_id: trade_id.to_string(),
                blackout_id: blackout_id.to_string(),
                reason: reason.map(str::to_string),
                set_at: now,
                expires_at: now + chrono::Duration::seconds(ttl_seconds as i64),
            };
            let body = serde_json::to_string(&entry)
                .map_err(|e| StateError::Backend(format!("encode pause: {e}")))?;
            self.put(key, body, ttl_seconds.max(MIN_TTL_SECONDS), now);
            Ok(())
        }

        async fn list_pauses_for_trade(
            &self,
            trade_id: &str,
        ) -> Result<Vec<PauseEntry>, StateError> {
            let prefix = format!("pause:{trade_id}:");
            let now = Utc::now();
            let inner = self.inner.borrow();
            let mut out = Vec::new();
            for (key, (val, exp)) in inner.iter() {
                if !key.starts_with(&prefix) || *exp <= now {
                    continue;
                }
                let entry: PauseEntry = serde_json::from_str(val)
                    .map_err(|e| StateError::Backend(format!("decode pause: {e}")))?;
                out.push(entry);
            }
            Ok(out)
        }

        async fn clear_pause(&self, trade_id: &str, blackout_id: &str) -> Result<bool, StateError> {
            let key = format!("pause:{trade_id}:{blackout_id}");
            Ok(self.delete(&key))
        }

        async fn set_news_window(
            &self,
            trade_id: &str,
            news_id: &str,
            reason: Option<&str>,
            now: DateTime<Utc>,
            ttl_seconds: u64,
        ) -> Result<(), StateError> {
            let key = format!("news:{trade_id}:{news_id}");
            let entry = NewsEntry {
                trade_id: trade_id.to_string(),
                news_id: news_id.to_string(),
                reason: reason.map(str::to_string),
                set_at: now,
                expires_at: now + chrono::Duration::seconds(ttl_seconds as i64),
            };
            let body = serde_json::to_string(&entry)
                .map_err(|e| StateError::Backend(format!("encode news: {e}")))?;
            self.put(key, body, ttl_seconds.max(MIN_TTL_SECONDS), now);
            Ok(())
        }

        async fn list_news_windows_for_trade(
            &self,
            trade_id: &str,
        ) -> Result<Vec<NewsEntry>, StateError> {
            let prefix = format!("news:{trade_id}:");
            let now = Utc::now();
            let inner = self.inner.borrow();
            let mut out = Vec::new();
            for (key, (val, exp)) in inner.iter() {
                if !key.starts_with(&prefix) || *exp <= now {
                    continue;
                }
                let entry: NewsEntry = serde_json::from_str(val)
                    .map_err(|e| StateError::Backend(format!("decode news: {e}")))?;
                out.push(entry);
            }
            Ok(out)
        }

        async fn clear_news_window(
            &self,
            trade_id: &str,
            news_id: &str,
        ) -> Result<bool, StateError> {
            let key = format!("news:{trade_id}:{news_id}");
            Ok(self.delete(&key))
        }

        async fn record_entry_attempt(&self, attempt: EntryAttempt) -> Result<(), StateError> {
            let scope = account_scope(attempt.account.as_deref()).to_string();
            let key = (scope, attempt.trade_id.clone());
            let mut attempts = self.attempts.borrow_mut();
            let list = attempts.entry(key).or_default();
            list.push(attempt);
            list.sort_by_key(|a| a.attempt_no);
            Ok(())
        }

        async fn list_entry_attempts(
            &self,
            account: Option<&str>,
            trade_id: &str,
        ) -> Result<Vec<EntryAttempt>, StateError> {
            let scope = account_scope(account).to_string();
            let key = (scope, trade_id.to_string());
            let attempts = self.attempts.borrow();
            Ok(attempts.get(&key).cloned().unwrap_or_default())
        }

        async fn set_entry_attempt_broker_trade_id(
            &self,
            account: Option<&str>,
            trade_id: &str,
            attempt_no: u32,
            broker_trade_id: &str,
        ) -> Result<(), StateError> {
            let scope = account_scope(account).to_string();
            let key = (scope, trade_id.to_string());
            let mut attempts = self.attempts.borrow_mut();
            if let Some(list) = attempts.get_mut(&key)
                && let Some(row) = list.iter_mut().find(|a| a.attempt_no == attempt_no)
            {
                row.broker_trade_id = Some(broker_trade_id.to_string());
            }
            Ok(())
        }

        async fn list_all_entry_attempts(&self) -> Result<Vec<EntryAttempt>, StateError> {
            Ok(self
                .attempts
                .borrow()
                .values()
                .flat_map(|list| list.iter().cloned())
                .collect())
        }

        async fn delete_entry_attempt(
            &self,
            account: Option<&str>,
            trade_id: &str,
            attempt_no: u32,
        ) -> Result<(), StateError> {
            let scope = account_scope(account).to_string();
            let key = (scope, trade_id.to_string());
            if let Some(list) = self.attempts.borrow_mut().get_mut(&key) {
                list.retain(|a| a.attempt_no != attempt_no);
            }
            Ok(())
        }

        async fn is_retry_fire_seen(
            &self,
            account: Option<&str>,
            trade_id: &str,
            shell_time: DateTime<Utc>,
        ) -> Result<bool, StateError> {
            let scope = account_scope(account);
            let key = format!("seen-retry:{scope}:{trade_id}:{}", shell_time.to_rfc3339());
            Ok(self.get_live(&key, Utc::now()).is_some())
        }

        async fn mark_retry_fire_seen(
            &self,
            account: Option<&str>,
            trade_id: &str,
            shell_time: DateTime<Utc>,
            ttl_seconds: u64,
        ) -> Result<(), StateError> {
            let scope = account_scope(account);
            let key = format!("seen-retry:{scope}:{trade_id}:{}", shell_time.to_rfc3339());
            self.put(
                key,
                "1".into(),
                ttl_seconds.max(MIN_TTL_SECONDS),
                Utc::now(),
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn veto_ttl_uses_bare_ttl_when_not_after_already_passed() {
        // not_after is in the past — the veto effectively starts "now",
        // so the bare TTL applies (12h × 3600).
        let now = ts("2026-05-20T10:00:00Z");
        let not_after = ts("2026-05-20T09:00:00Z");
        assert_eq!(veto_ttl_seconds(12, not_after, now), 12 * 3600);
    }

    #[test]
    fn veto_ttl_extends_when_not_after_in_future() {
        // not_after = now + 8h, ttl = 12h → expect 20h.
        let now = ts("2026-05-20T10:00:00Z");
        let not_after = ts("2026-05-20T18:00:00Z");
        assert_eq!(veto_ttl_seconds(12, not_after, now), 20 * 3600);
    }

    #[test]
    fn veto_ttl_uses_bare_ttl_when_not_after_equals_now() {
        // Boundary: not_after exactly == now → remaining = 0 → bare TTL.
        let now = ts("2026-05-20T10:00:00Z");
        assert_eq!(veto_ttl_seconds(6, now, now), 6 * 3600);
    }

    #[test]
    fn veto_ttl_clamps_to_min_ttl() {
        // ttl_hours = 0 with not_after in the past → would be 0 seconds,
        // but Cloudflare KV requires at least 60s.
        let now = ts("2026-05-20T10:00:00Z");
        let past = ts("2026-05-20T09:00:00Z");
        assert_eq!(veto_ttl_seconds(0, past, now), MIN_TTL_SECONDS);
    }

    fn cd(instrument: &str, expires_at: DateTime<Utc>) -> CooldownEntry {
        CooldownEntry {
            instrument: instrument.into(),
            set_at: None,
            expires_at,
            account: None,
        }
    }

    fn se(id: &str, expires_at: DateTime<Utc>) -> SeenEntry {
        SeenEntry {
            id: id.into(),
            action: Action::Enter,
            seen_at: None,
            outcome: String::new(),
            expires_at,
            trade_id: None,
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
        pollster::block_on(store.mark_seen("abc", Action::Prep, Utc::now(), "ok", 3600, None))
            .unwrap();
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
        pollster::block_on(store.mark_seen(
            "retest-msg-id",
            Action::Prep,
            now,
            "ok",
            24 * 3600,
            None,
        ))
        .unwrap();
        pollster::block_on(store.set_prep(None, "EUR_USD", "retest", now, 3600, "retest-msg-id"))
            .unwrap();

        // Clearing the prep via clear_named_preps should also drop
        // the seen record.
        let cleared = pollster::block_on(clear_named_preps(
            &store,
            None,
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
        pollster::block_on(store.set_prep(None, "EUR_USD", "break", now, 4 * 3600, "setter-1"))
            .unwrap();
        let got = pollster::block_on(store.get_prep(None, "EUR_USD", "break")).unwrap();
        assert_eq!(got, Some(now));
        let cleared = pollster::block_on(store.clear_prep(None, "EUR_USD", "break")).unwrap();
        assert_eq!(cleared.as_deref(), Some("setter-1"));
        let got = pollster::block_on(store.get_prep(None, "EUR_USD", "break")).unwrap();
        assert!(got.is_none());
        // Clearing again returns None — the prep is gone.
        let again = pollster::block_on(store.clear_prep(None, "EUR_USD", "break")).unwrap();
        assert!(again.is_none());
    }

    #[test]
    fn memstore_get_prep_absent() {
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        let got = pollster::block_on(store.get_prep(None, "EUR_USD", "ghost")).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn memstore_prep_scoped_per_account() {
        // The bug-fix case for preps: a prep landed for one account is
        // not relevant to a different account's setup on the same pair.
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        let now = Utc::now();
        pollster::block_on(store.set_prep(
            Some("acct-a"),
            "EUR_USD",
            "break-and-close",
            now,
            3600,
            "id-a",
        ))
        .unwrap();
        assert_eq!(
            pollster::block_on(store.get_prep(Some("acct-a"), "EUR_USD", "break-and-close"))
                .unwrap(),
            Some(now),
            "prep should be visible on the account that set it"
        );
        assert_eq!(
            pollster::block_on(store.get_prep(Some("acct-b"), "EUR_USD", "break-and-close"))
                .unwrap(),
            None,
            "prep on acct-a must NOT satisfy the entry gate on acct-b — bug fix"
        );
        assert_eq!(
            pollster::block_on(store.get_prep(None, "EUR_USD", "break-and-close")).unwrap(),
            None,
            "a scoped prep must not register as a global prep"
        );
    }

    #[test]
    fn memstore_prep_clear_is_scoped() {
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        let now = Utc::now();
        pollster::block_on(store.set_prep(Some("acct-a"), "EUR_USD", "break", now, 3600, "id-a"))
            .unwrap();
        pollster::block_on(store.set_prep(Some("acct-b"), "EUR_USD", "break", now, 3600, "id-b"))
            .unwrap();
        // Clearing on acct-a returns Some(setter_id) and removes only
        // that scope. acct-b's prep is untouched.
        let cleared =
            pollster::block_on(store.clear_prep(Some("acct-a"), "EUR_USD", "break")).unwrap();
        assert_eq!(cleared.as_deref(), Some("id-a"));
        assert!(
            pollster::block_on(store.get_prep(Some("acct-b"), "EUR_USD", "break"))
                .unwrap()
                .is_some(),
            "acct-b's prep must survive an acct-a clear"
        );
    }

    #[test]
    fn memstore_cooldown_scoped_per_account() {
        // Mirror of the veto scoping test for cooldowns.
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        let now = Utc::now();
        pollster::block_on(store.set_cooldown(Some("acct-a"), "EUR_USD", 1, now)).unwrap();
        assert!(pollster::block_on(store.is_cooled_down(Some("acct-a"), "EUR_USD")).unwrap());
        assert!(
            !pollster::block_on(store.is_cooled_down(Some("acct-b"), "EUR_USD")).unwrap(),
            "a cooldown on acct-a must not pause acct-b"
        );
        assert!(!pollster::block_on(store.is_cooled_down(None, "EUR_USD")).unwrap());
    }

    #[test]
    fn memstore_global_cooldown_pauses_every_account() {
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        let now = Utc::now();
        pollster::block_on(store.set_cooldown(None, "EUR_USD", 1, now)).unwrap();
        assert!(pollster::block_on(store.is_cooled_down(Some("acct-a"), "EUR_USD")).unwrap());
        assert!(pollster::block_on(store.is_cooled_down(Some("acct-b"), "EUR_USD")).unwrap());
    }

    #[test]
    fn memstore_global_prep_satisfies_every_account() {
        // Symmetric with the global-veto + global-cooldown behaviour: a
        // `None` prep is visible to every account. Unlikely to be used
        // in practice (preps are setup-specific), but kept for shape
        // consistency.
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        let now = Utc::now();
        pollster::block_on(store.set_prep(None, "EUR_USD", "break", now, 3600, "id-g")).unwrap();
        assert_eq!(
            pollster::block_on(store.get_prep(Some("acct-a"), "EUR_USD", "break")).unwrap(),
            Some(now)
        );
    }

    #[test]
    fn memstore_veto_round_trip() {
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        // Global veto (no account scope) — back-compat with single-account workers.
        pollster::block_on(store.set_veto(None, "EUR_USD", "news-window", 6 * 3600)).unwrap();
        assert!(pollster::block_on(store.is_vetoed(None, "EUR_USD", "news-window")).unwrap());
        let was = pollster::block_on(store.clear_veto(None, "EUR_USD", "news-window")).unwrap();
        assert!(was);
        assert!(!pollster::block_on(store.is_vetoed(None, "EUR_USD", "news-window")).unwrap());
    }

    #[test]
    fn memstore_veto_scoped_per_account() {
        // The bug-fix case: a veto on account A must not block account B
        // on the same instrument. Both accounts trade EUR_USD; account A
        // sets `news`. Account B should be unaffected.
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        pollster::block_on(store.set_veto(Some("acct-a"), "EUR_USD", "news", 6 * 3600)).unwrap();
        assert!(
            pollster::block_on(store.is_vetoed(Some("acct-a"), "EUR_USD", "news")).unwrap(),
            "veto should be active on the account that set it"
        );
        assert!(
            !pollster::block_on(store.is_vetoed(Some("acct-b"), "EUR_USD", "news")).unwrap(),
            "veto on acct-a must NOT bleed into acct-b — this is the bug we're fixing"
        );
        assert!(
            !pollster::block_on(store.is_vetoed(None, "EUR_USD", "news")).unwrap(),
            "a scoped veto must not register as a global veto"
        );
    }

    #[test]
    fn memstore_global_veto_covers_every_account() {
        // The other side of the scoping rule: a worker-wide veto
        // (`account = None`) does affect every account. The CLI keeps
        // this as a deliberate kill-switch.
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        pollster::block_on(store.set_veto(None, "EUR_USD", "halt", 6 * 3600)).unwrap();
        assert!(pollster::block_on(store.is_vetoed(Some("acct-a"), "EUR_USD", "halt")).unwrap());
        assert!(pollster::block_on(store.is_vetoed(Some("acct-b"), "EUR_USD", "halt")).unwrap());
    }

    #[test]
    fn memstore_veto_clear_is_scoped() {
        // Clearing on one account doesn't touch another account's veto
        // or the global veto with the same name.
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        let ttl = 6 * 3600;
        pollster::block_on(store.set_veto(Some("acct-a"), "EUR_USD", "news", ttl)).unwrap();
        pollster::block_on(store.set_veto(Some("acct-b"), "EUR_USD", "news", ttl)).unwrap();
        pollster::block_on(store.set_veto(None, "EUR_USD", "news", ttl)).unwrap();

        let was = pollster::block_on(store.clear_veto(Some("acct-a"), "EUR_USD", "news")).unwrap();
        assert!(was);
        // After clearing on acct-a, acct-b still sees a veto (its own
        // scoped veto AND the global). Switch out the global so we can
        // verify acct-b's specifically is left in place.
        assert!(pollster::block_on(store.clear_veto(None, "EUR_USD", "news")).unwrap());
        assert!(
            pollster::block_on(store.is_vetoed(Some("acct-b"), "EUR_USD", "news")).unwrap(),
            "acct-b's scoped veto must survive both an acct-a clear and a global clear"
        );
    }

    #[test]
    fn memstore_preps_per_instrument_are_independent() {
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        let t1 = Utc::now();
        let t2 = t1 + chrono::Duration::minutes(5);
        pollster::block_on(store.set_prep(None, "EUR_USD", "break", t1, 3600, "id-1")).unwrap();
        pollster::block_on(store.set_prep(None, "USD_JPY", "break", t2, 3600, "id-2")).unwrap();
        assert_eq!(
            pollster::block_on(store.get_prep(None, "EUR_USD", "break")).unwrap(),
            Some(t1)
        );
        assert_eq!(
            pollster::block_on(store.get_prep(None, "USD_JPY", "break")).unwrap(),
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
        pollster::block_on(store.set_prep(None, "EUR_USD", "break", t1, ttl, "id-a")).unwrap();
        pollster::block_on(store.set_prep(None, "EUR_USD", "break", t2, ttl, "id-b")).unwrap();
        // Refiring a prep refreshes its timestamp — documented behaviour.
        assert_eq!(
            pollster::block_on(store.get_prep(None, "EUR_USD", "break")).unwrap(),
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
        pollster::block_on(store.set_prep(None, "EUR_USD", "retest", now, ttl, "retest-id"))
            .unwrap();
        pollster::block_on(store.set_prep(None, "EUR_USD", "other", now, ttl, "other-id")).unwrap();

        let cleared = pollster::block_on(clear_named_preps(
            &store,
            None,
            "EUR_USD",
            &["retest".to_string(), "ghost".to_string()],
        ))
        .unwrap();
        // `retest` was present; `ghost` was not. Only the present one
        // is reported in the cleared set.
        assert_eq!(cleared, vec!["retest".to_string()]);

        // Untargeted prep survives.
        assert!(
            pollster::block_on(store.get_prep(None, "EUR_USD", "other"))
                .unwrap()
                .is_some()
        );
        // Targeted prep is gone.
        assert!(
            pollster::block_on(store.get_prep(None, "EUR_USD", "retest"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn clear_named_preps_on_empty_list_is_a_noop() {
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        let now = Utc::now();
        pollster::block_on(store.set_prep(None, "EUR_USD", "retest", now, 24 * 3600, "retest-id"))
            .unwrap();
        let cleared = pollster::block_on(clear_named_preps(&store, None, "EUR_USD", &[])).unwrap();
        assert!(cleared.is_empty());
        // Existing prep untouched.
        assert!(
            pollster::block_on(store.get_prep(None, "EUR_USD", "retest"))
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
        pollster::block_on(store.set_prep(None, "EUR_USD", "retest", now, ttl, "eur-id")).unwrap();
        pollster::block_on(store.set_prep(None, "USD_JPY", "retest", now, ttl, "jpy-id")).unwrap();

        let cleared = pollster::block_on(clear_named_preps(
            &store,
            None,
            "EUR_USD",
            &["retest".to_string()],
        ))
        .unwrap();
        assert_eq!(cleared, vec!["retest".to_string()]);
        assert!(
            pollster::block_on(store.get_prep(None, "EUR_USD", "retest"))
                .unwrap()
                .is_none()
        );
        // USD_JPY untouched.
        assert!(
            pollster::block_on(store.get_prep(None, "USD_JPY", "retest"))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn clear_named_vetos_removes_only_listed_names() {
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        let ttl = 24 * 3600;
        pollster::block_on(store.set_veto(None, "EUR_USD", "news", ttl)).unwrap();
        pollster::block_on(store.set_veto(None, "EUR_USD", "other", ttl)).unwrap();

        let cleared = pollster::block_on(clear_named_vetos(
            &store,
            None,
            "EUR_USD",
            &["news".to_string()],
        ))
        .unwrap();
        assert_eq!(cleared, vec!["news".to_string()]);
        assert!(!pollster::block_on(store.is_vetoed(None, "EUR_USD", "news")).unwrap());
        assert!(pollster::block_on(store.is_vetoed(None, "EUR_USD", "other")).unwrap());
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
                account: None,
            },
            PrepEntry {
                instrument: "EUR_USD".into(),
                step: "fresh".into(),
                set_at: ts("2026-05-14T11:30:00Z"),
                expires_at: ts("2026-05-14T15:00:00Z"), // live
                account: None,
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
                account: None,
            },
            VetoEntry {
                instrument: "USD_JPY".into(),
                name: "fresh".into(),
                expires_at: ts("2026-05-14T13:00:00Z"),
                account: None,
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
                account: Some("oanda-reversals-demo".into()),
            }],
            vetos: vec![VetoEntry {
                instrument: "EUR_USD".into(),
                name: "news-window".into(),
                expires_at: ts("2026-05-14T13:00:00Z"),
                account: Some("oanda-reversals-demo".into()),
            }],
            pauses: vec![PauseEntry {
                trade_id: "eurusd-hs-1".into(),
                blackout_id: "nfp-2026-06-06".into(),
                reason: Some("news:USD-NFP".into()),
                set_at: ts("2026-05-14T11:00:00Z"),
                expires_at: ts("2026-05-15T11:00:00Z"),
            }],
            news_windows: vec![NewsEntry {
                trade_id: "eurusd-hs-1".into(),
                news_id: "usd-nfp-2026-06-06".into(),
                reason: Some("USD-NFP".into()),
                set_at: ts("2026-05-14T11:30:00Z"),
                expires_at: ts("2026-05-14T12:30:00Z"),
            }],
        };
        let yaml = serde_yaml::to_string(&snap).unwrap();
        assert!(yaml.contains("preps:"));
        assert!(yaml.contains("step: break-and-close"));
        assert!(yaml.contains("vetos:"));
        assert!(yaml.contains("name: news-window"));
        assert!(yaml.contains("pauses:"));
        assert!(yaml.contains("blackout_id: nfp-2026-06-06"));
        assert!(yaml.contains("news_windows:"));
        assert!(yaml.contains("news_id: usd-nfp-2026-06-06"));
    }

    #[test]
    fn snapshot_deserialises_without_new_sections_for_back_compat() {
        // Pre-existing serialised snapshots (e.g. in unit tests, or any
        // stored copies) have no `preps:` / `vetos:` / `pauses:` /
        // `news_windows:` fields. Make sure they still parse — the new
        // fields default to empty.
        let yaml = "now: \"2026-05-14T12:00:00Z\"\ncooldowns: []\nrecent_seen: []\n";
        let snap: Snapshot = serde_yaml::from_str(yaml).unwrap();
        assert!(snap.preps.is_empty());
        assert!(snap.vetos.is_empty());
        assert!(snap.pauses.is_empty());
        assert!(snap.news_windows.is_empty());
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
        // Pre-set_at cooldowns must still parse, but the new `account`
        // field is required (no on-disk back-compat — KV is wiped at
        // deploy). Explicit None matches what an un-deployed entry
        // would look like after a one-off migration.
        let yaml = "instrument: EUR_USD\nexpires_at: \"2026-05-14T13:00:00Z\"\naccount: null\n";
        let entry: CooldownEntry = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(entry.instrument, "EUR_USD");
        assert_eq!(entry.set_at, None);
        assert_eq!(entry.account, None);
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
            trade_id: None,
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

    fn attempt(
        trade_id: &str,
        account: Option<&str>,
        attempt_no: u32,
        broker_order_id: &str,
    ) -> EntryAttempt {
        let now = Utc::now();
        EntryAttempt {
            trade_id: trade_id.into(),
            account: account.map(|s| s.to_string()),
            instrument: "EUR_USD".into(),
            attempt_no,
            broker_order_id: broker_order_id.into(),
            broker_trade_id: None,
            direction: Direction::Long,
            placed_at: now,
            shell_time: now,
            expires_at: now + chrono::Duration::hours(24),
            stop_loss_price: None,
        }
    }

    #[test]
    fn memstore_entry_attempt_round_trip() {
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        let a = attempt("tid-1", None, 1, "ord-1");
        pollster::block_on(store.record_entry_attempt(a.clone())).unwrap();
        let got = pollster::block_on(store.list_entry_attempts(None, "tid-1")).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].broker_order_id, "ord-1");
        assert_eq!(got[0].attempt_no, 1);
    }

    #[test]
    fn memstore_list_entry_attempts_orders_by_attempt_no() {
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        // Insert out of order; expect them returned sorted.
        pollster::block_on(store.record_entry_attempt(attempt("tid-2", None, 3, "ord-3"))).unwrap();
        pollster::block_on(store.record_entry_attempt(attempt("tid-2", None, 1, "ord-1"))).unwrap();
        pollster::block_on(store.record_entry_attempt(attempt("tid-2", None, 2, "ord-2"))).unwrap();
        let got = pollster::block_on(store.list_entry_attempts(None, "tid-2")).unwrap();
        assert_eq!(
            got.iter().map(|a| a.attempt_no).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn memstore_entry_attempts_isolated_per_account() {
        // Same trade_id on two different accounts must not interfere.
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        pollster::block_on(store.record_entry_attempt(attempt(
            "tid-shared",
            Some("acct-a"),
            1,
            "ord-a1",
        )))
        .unwrap();
        pollster::block_on(store.record_entry_attempt(attempt(
            "tid-shared",
            Some("acct-b"),
            1,
            "ord-b1",
        )))
        .unwrap();
        let a =
            pollster::block_on(store.list_entry_attempts(Some("acct-a"), "tid-shared")).unwrap();
        let b =
            pollster::block_on(store.list_entry_attempts(Some("acct-b"), "tid-shared")).unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(b.len(), 1);
        assert_eq!(a[0].broker_order_id, "ord-a1");
        assert_eq!(b[0].broker_order_id, "ord-b1");
        // And the global scope is empty — scoped attempts must not bleed.
        let g = pollster::block_on(store.list_entry_attempts(None, "tid-shared")).unwrap();
        assert!(g.is_empty());
    }

    #[test]
    fn memstore_set_entry_attempt_broker_trade_id() {
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        pollster::block_on(store.record_entry_attempt(attempt("tid-3", None, 1, "ord-1"))).unwrap();
        pollster::block_on(store.set_entry_attempt_broker_trade_id(None, "tid-3", 1, "trade-xyz"))
            .unwrap();
        let got = pollster::block_on(store.list_entry_attempts(None, "tid-3")).unwrap();
        assert_eq!(got[0].broker_trade_id.as_deref(), Some("trade-xyz"));
        // Idempotent: setting again is fine.
        pollster::block_on(store.set_entry_attempt_broker_trade_id(None, "tid-3", 1, "trade-xyz"))
            .unwrap();
        // Setting a non-existent attempt_no is a no-op (silent).
        pollster::block_on(store.set_entry_attempt_broker_trade_id(None, "tid-3", 99, "trade-zzz"))
            .unwrap();
        let got = pollster::block_on(store.list_entry_attempts(None, "tid-3")).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].broker_trade_id.as_deref(), Some("trade-xyz"));
    }

    #[test]
    fn memstore_retry_fire_seen_round_trip() {
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        let shell_time = ts("2026-05-20T10:00:00Z");
        // Not seen initially.
        assert!(!pollster::block_on(store.is_retry_fire_seen(None, "tid-r", shell_time)).unwrap());
        pollster::block_on(store.mark_retry_fire_seen(None, "tid-r", shell_time, 3600)).unwrap();
        assert!(pollster::block_on(store.is_retry_fire_seen(None, "tid-r", shell_time)).unwrap());
        // A different shell_time is independent.
        let other = ts("2026-05-20T11:00:00Z");
        assert!(!pollster::block_on(store.is_retry_fire_seen(None, "tid-r", other)).unwrap());
    }

    #[test]
    fn memstore_retry_fire_seen_isolated_per_account() {
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        let shell_time = ts("2026-05-20T10:00:00Z");
        pollster::block_on(store.mark_retry_fire_seen(Some("acct-a"), "tid-r", shell_time, 3600))
            .unwrap();
        assert!(
            pollster::block_on(store.is_retry_fire_seen(Some("acct-a"), "tid-r", shell_time))
                .unwrap()
        );
        assert!(
            !pollster::block_on(store.is_retry_fire_seen(Some("acct-b"), "tid-r", shell_time))
                .unwrap()
        );
        assert!(!pollster::block_on(store.is_retry_fire_seen(None, "tid-r", shell_time)).unwrap());
    }

    #[test]
    fn entry_attempt_round_trips_through_yaml() {
        let now = ts("2026-05-20T10:00:00Z");
        let a = EntryAttempt {
            trade_id: "eurusd-long-mr".into(),
            account: Some("acct-a".into()),
            instrument: "EUR_USD".into(),
            attempt_no: 2,
            broker_order_id: "ord-42".into(),
            broker_trade_id: Some("trade-42".into()),
            direction: Direction::Long,
            placed_at: now,
            shell_time: now,
            expires_at: now + chrono::Duration::hours(24),
            stop_loss_price: Some(1.0500),
        };
        let yaml = serde_yaml::to_string(&a).unwrap();
        let parsed: EntryAttempt = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, a);
    }

    #[test]
    fn entry_attempt_decodes_pre_stop_loss_json() {
        // Pre-`stop_loss_price` serialised rows (everything written
        // before the cron sweep landed) lack the field entirely. They
        // must decode with `stop_loss_price = None` so the sweep
        // treats them as "skip the breach check" and lets the row
        // expire via TTL.
        let json = r#"{
            "trade_id":"eurusd-long-mr",
            "account":"acct-a",
            "instrument":"EUR_USD",
            "attempt_no":1,
            "broker_order_id":"ord-1",
            "direction":"long",
            "placed_at":"2026-05-20T10:00:00Z",
            "shell_time":"2026-05-20T10:00:00Z",
            "expires_at":"2026-05-21T10:00:00Z"
        }"#;
        let attempt: EntryAttempt = serde_json::from_str(json).unwrap();
        assert!(attempt.stop_loss_price.is_none());
        assert_eq!(attempt.broker_order_id, "ord-1");
    }

    #[test]
    fn entry_attempt_omits_broker_trade_id_when_none() {
        let now = ts("2026-05-20T10:00:00Z");
        let a = EntryAttempt {
            trade_id: "eurusd-long-mr".into(),
            account: None,
            instrument: "EUR_USD".into(),
            attempt_no: 1,
            broker_order_id: "ord-1".into(),
            broker_trade_id: None,
            direction: Direction::Long,
            placed_at: now,
            shell_time: now,
            expires_at: now + chrono::Duration::hours(24),
            stop_loss_price: None,
        };
        let yaml = serde_yaml::to_string(&a).unwrap();
        assert!(!yaml.contains("broker_trade_id"));
        let parsed: EntryAttempt = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.broker_trade_id, None);
    }

    #[test]
    fn memstore_pause_round_trip_lists_then_clears() {
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        let now = Utc::now();
        pollster::block_on(store.set_pause(
            "eurusd-hs-1",
            "nfp-2026-06-06",
            Some("news:USD-NFP"),
            now,
            6 * 3600,
        ))
        .unwrap();
        let listed = pollster::block_on(store.list_pauses_for_trade("eurusd-hs-1")).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].blackout_id, "nfp-2026-06-06");
        assert_eq!(listed[0].reason.as_deref(), Some("news:USD-NFP"));
        let was = pollster::block_on(store.clear_pause("eurusd-hs-1", "nfp-2026-06-06")).unwrap();
        assert!(was);
        let listed = pollster::block_on(store.list_pauses_for_trade("eurusd-hs-1")).unwrap();
        assert!(listed.is_empty());
        // Clearing an absent pause is a no-op.
        let was = pollster::block_on(store.clear_pause("eurusd-hs-1", "nfp-2026-06-06")).unwrap();
        assert!(!was);
    }

    #[test]
    fn memstore_pause_multiple_blackouts_per_trade() {
        // Two concurrent blackouts on one trade (e.g. NFP + central-bank
        // decision) must coexist. Clearing one leaves the other active —
        // which is the whole point of the per-blackout_id scoping.
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        let now = Utc::now();
        pollster::block_on(store.set_pause(
            "eurusd-hs-1",
            "nfp",
            Some("news:USD-NFP"),
            now,
            6 * 3600,
        ))
        .unwrap();
        pollster::block_on(store.set_pause(
            "eurusd-hs-1",
            "cb-rate",
            Some("news:ECB-rate"),
            now,
            6 * 3600,
        ))
        .unwrap();
        let listed = pollster::block_on(store.list_pauses_for_trade("eurusd-hs-1")).unwrap();
        assert_eq!(listed.len(), 2);

        // Clear only NFP — cb-rate must remain.
        let was = pollster::block_on(store.clear_pause("eurusd-hs-1", "nfp")).unwrap();
        assert!(was);
        let listed = pollster::block_on(store.list_pauses_for_trade("eurusd-hs-1")).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].blackout_id, "cb-rate");
    }

    #[test]
    fn memstore_pause_isolated_per_trade_id() {
        // A pause on trade A must not be visible when querying trade B.
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        let now = Utc::now();
        pollster::block_on(store.set_pause("trade-a", "b1", None, now, 6 * 3600)).unwrap();
        let a = pollster::block_on(store.list_pauses_for_trade("trade-a")).unwrap();
        let b = pollster::block_on(store.list_pauses_for_trade("trade-b")).unwrap();
        assert_eq!(a.len(), 1);
        assert!(b.is_empty());
    }

    #[test]
    fn memstore_news_round_trip_lists_then_clears() {
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        let now = Utc::now();
        pollster::block_on(store.set_news_window(
            "eurusd-hs-1",
            "usd-nfp-2026-06-06",
            Some("USD-NFP"),
            now,
            3600,
        ))
        .unwrap();
        let listed = pollster::block_on(store.list_news_windows_for_trade("eurusd-hs-1")).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].news_id, "usd-nfp-2026-06-06");
        assert_eq!(listed[0].reason.as_deref(), Some("USD-NFP"));
        let was = pollster::block_on(store.clear_news_window("eurusd-hs-1", "usd-nfp-2026-06-06"))
            .unwrap();
        assert!(was);
        let listed = pollster::block_on(store.list_news_windows_for_trade("eurusd-hs-1")).unwrap();
        assert!(listed.is_empty());
        // Clearing an absent window is a no-op.
        let was = pollster::block_on(store.clear_news_window("eurusd-hs-1", "usd-nfp-2026-06-06"))
            .unwrap();
        assert!(!was);
    }

    #[test]
    fn memstore_news_isolated_from_pauses() {
        // News windows live in their own namespace — setting a pause
        // must not show up in the news listing and vice versa. The
        // KV keys (`pause:` vs `news:`) keep them separate.
        use super::memstore::MemStateStore;
        let store = MemStateStore::new();
        let now = Utc::now();
        pollster::block_on(store.set_pause("trade-a", "nfp", None, now, 3600)).unwrap();
        pollster::block_on(store.set_news_window("trade-a", "nfp", None, now, 3600)).unwrap();
        // Both visible only via their own listing.
        let pauses = pollster::block_on(store.list_pauses_for_trade("trade-a")).unwrap();
        let news = pollster::block_on(store.list_news_windows_for_trade("trade-a")).unwrap();
        assert_eq!(pauses.len(), 1);
        assert_eq!(news.len(), 1);
        // Clearing the news window must not touch the pause.
        pollster::block_on(store.clear_news_window("trade-a", "nfp")).unwrap();
        let pauses = pollster::block_on(store.list_pauses_for_trade("trade-a")).unwrap();
        let news = pollster::block_on(store.list_news_windows_for_trade("trade-a")).unwrap();
        assert_eq!(pauses.len(), 1);
        assert!(news.is_empty());
    }

    #[test]
    fn cooldown_entry_serialises_with_set_at() {
        let entry = CooldownEntry {
            instrument: "F40".into(),
            set_at: Some(ts("2026-05-15T18:00:34Z")),
            expires_at: ts("2026-05-16T06:00:34Z"),
            account: Some("oanda-reversals-demo".into()),
        };
        let yaml = serde_yaml::to_string(&entry).unwrap();
        let parsed: CooldownEntry = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.instrument, "F40");
        assert_eq!(parsed.set_at, Some(ts("2026-05-15T18:00:34Z")));
        assert_eq!(parsed.expires_at, ts("2026-05-16T06:00:34Z"));
        assert_eq!(parsed.account, Some("oanda-reversals-demo".into()));
    }
}
