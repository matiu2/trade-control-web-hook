//! Cloudflare KV-backed `StateStore`.
//!
//! Replay protection (`seen:<id>`) and cooldowns (`cooldown:<instrument>`) are
//! stored as TTL keys whose presence is authoritative. To answer the `status`
//! action — which needs to *list* current entries, something the Workers KV
//! SDK doesn't expose — a parallel JSON-encoded index is maintained at
//! `index:cooldowns` and `index:seen`. Indexes are pruned lazily on read and
//! write; they are "best effort" (concurrent writers can race their RMW) but
//! the TTL keys never lie.

use chrono::{DateTime, Utc};
use trade_control_core::intent::Action;
use trade_control_core::plan_state::PlanState;
use trade_control_core::state::{
    CooldownEntry, EntryAttempt, MIN_TTL_SECONDS, MwState, NewsEntry, PREP_BLOCK_INDEX_CAP,
    PREP_INDEX_CAP, PauseEntry, PrepBlockEntry, PrepEntry, SEEN_INDEX_CAP, SeenEntry, Snapshot,
    SpreadBlackoutRecord, SpreadBlackoutWindow, StateError, StateStore, StoredPlan, VETO_INDEX_CAP,
    VetoEntry, account_from_scope, account_scope, prune_expired,
};
use trade_control_core::trade_plan::TradePlan;
use worker::kv::KvStore;

const INDEX_COOLDOWNS_KEY: &str = "index:cooldowns";
const INDEX_SEEN_KEY: &str = "index:seen";
const INDEX_PREPS_KEY: &str = "index:preps";
const INDEX_VETOS_KEY: &str = "index:vetos";
const INDEX_PREP_BLOCKS_KEY: &str = "index:prep-blocks";

pub struct KvStateStore {
    store: KvStore,
}

impl KvStateStore {
    pub fn new(store: KvStore) -> Self {
        Self { store }
    }

    fn seen_key(id: &str) -> String {
        format!("seen:{id}")
    }

    fn cooldown_key(account: Option<&str>, instrument: &str) -> String {
        let scope = account_scope(account);
        format!("cooldown:{scope}:{instrument}")
    }

    fn prep_key(account: Option<&str>, instrument: &str, step: &str) -> String {
        let scope = account_scope(account);
        format!("prep:{scope}:{instrument}:{step}")
    }

    fn veto_key(account: Option<&str>, trade_id: &str, instrument: &str, name: &str) -> String {
        let scope = account_scope(account);
        format!("veto:{scope}:{trade_id}:{instrument}:{name}")
    }

    fn prep_block_key(account: Option<&str>, instrument: &str, step: &str) -> String {
        let scope = account_scope(account);
        format!("prep-blocked:{scope}:{instrument}:{step}")
    }

    fn entry_attempt_key(account: Option<&str>, trade_id: &str, attempt_no: u32) -> String {
        let scope = account_scope(account);
        format!("entry_attempt:{scope}:{trade_id}:{attempt_no}")
    }

    fn entry_attempt_prefix(account: Option<&str>, trade_id: &str) -> String {
        let scope = account_scope(account);
        format!("entry_attempt:{scope}:{trade_id}:")
    }

    fn retry_fire_seen_key(
        account: Option<&str>,
        trade_id: &str,
        shell_time: DateTime<Utc>,
    ) -> String {
        let scope = account_scope(account);
        format!("seen-retry:{scope}:{trade_id}:{}", shell_time.to_rfc3339())
    }

    fn pause_key(trade_id: &str, blackout_id: &str) -> String {
        format!("pause:{trade_id}:{blackout_id}")
    }

    /// Prefix used to list every blackout for a single `trade_id`. The
    /// trailing colon keeps `eurusd-hs-1` from accidentally matching
    /// `eurusd-hs-11` blackout entries.
    fn pause_trade_prefix(trade_id: &str) -> String {
        format!("pause:{trade_id}:")
    }

    /// Global prefix used by `snapshot()` to enumerate every active
    /// pause across every trade — no second `:` here, since the
    /// next segment is the trade_id.
    fn pause_all_prefix() -> &'static str {
        "pause:"
    }

    fn news_key(trade_id: &str, news_id: &str) -> String {
        format!("news:{trade_id}:{news_id}")
    }

    /// Singleton key for the global spread-blackout window marker.
    fn spread_blackout_window_key() -> &'static str {
        "spread-blackout:window"
    }

    /// Per-trade record key. The `rec:` segment keeps the record
    /// namespace cleanly separable from the singleton `window` key —
    /// a `list().prefix("spread-blackout:rec:")` never matches the
    /// window marker.
    fn spread_blackout_record_key(trade_id: &str) -> String {
        format!("spread-blackout:rec:{trade_id}")
    }

    /// Prefix used by the recovery watcher to enumerate every per-trade
    /// record. Disjoint from the window key by the `rec:` segment.
    fn spread_blackout_record_prefix() -> &'static str {
        "spread-blackout:rec:"
    }

    /// Per-(account, trade_id) key holding the evolving M/W geometry
    /// ([`MwState`]): the running right-shoulder + revised-neckline
    /// correction applied on top of the baked params. Scoped like vetos
    /// minus the instrument+name segments — `trade_id` is globally unique.
    fn mw_state_key(account: Option<&str>, trade_id: &str) -> String {
        let scope = account_scope(account);
        format!("mw-state:{scope}:{trade_id}")
    }

    /// Key for a registered server-side [`TradePlan`]: `plan:{scope}:{trade_id}`.
    /// The cron engine enumerates these by the `plan:` prefix and recovers the
    /// account from the `{scope}` segment.
    fn trade_plan_key(account: Option<&str>, trade_id: &str) -> String {
        let scope = account_scope(account);
        format!("plan:{scope}:{trade_id}")
    }

    /// Key for the engine's per-trade FSM state: `plan-state:{scope}:{trade_id}`.
    fn plan_state_key(account: Option<&str>, trade_id: &str) -> String {
        let scope = account_scope(account);
        format!("plan-state:{scope}:{trade_id}")
    }

    /// Per-order key holding the raw signed alert body that placed a broker
    /// order, keyed by `broker_order_id`. Written on successful single-shot
    /// placement (`run_enter`) and read by the spread-blackout apply cron,
    /// which finds a broker *pending order* and needs the original signed
    /// bytes to re-drive that entry on recovery via
    /// `incoming::parse_and_verify`. TTL'd to the alert window + grace.
    fn order_body_key(order_id: &str) -> String {
        format!("order:{order_id}")
    }

    /// Persist the raw signed alert body for a placed order so the
    /// spread-blackout apply cron can recover + re-drive it. `order_id` is
    /// the broker order id [`crate::tradenation_adapter`] / OANDA returned.
    /// Inherent (not on [`StateStore`]) because only the worker's entry path
    /// and the blackout crons touch it — keeping it off the trait avoids
    /// rippling into the in-memory test store + CLI.
    pub async fn put_order_body(
        &self,
        order_id: &str,
        signed_body: &str,
        ttl_seconds: u64,
    ) -> Result<(), StateError> {
        let key = Self::order_body_key(order_id);
        let ttl = ttl_seconds.max(MIN_TTL_SECONDS);
        self.store
            .put(&key, signed_body)
            .map_err(|e| StateError::Backend(format!("put {key} builder: {e:?}")))?
            .expiration_ttl(ttl)
            .execute()
            .await
            .map_err(|e| StateError::Backend(format!("put {key} execute: {e:?}")))?;
        Ok(())
    }

    /// Read the raw signed alert body for `order_id`, or `None` if absent /
    /// TTL-expired (the order's alert window closed before any blackout, so
    /// it can't and shouldn't be restored).
    pub async fn get_order_body(&self, order_id: &str) -> Result<Option<String>, StateError> {
        let key = Self::order_body_key(order_id);
        self.store
            .get(&key)
            .text()
            .await
            .map_err(|e| StateError::Backend(format!("get {key}: {e:?}")))
    }

    /// Best-effort delete of an order-body row once it has been re-driven on
    /// recovery (or the order is otherwise gone). A no-op if already expired.
    pub async fn delete_order_body(&self, order_id: &str) -> Result<(), StateError> {
        let key = Self::order_body_key(order_id);
        self.store
            .delete(&key)
            .await
            .map_err(|e| StateError::Backend(format!("delete {key}: {e:?}")))?;
        Ok(())
    }

    /// Prefix used to list every news window for a single `trade_id`.
    /// Trailing colon for the same reason as [`Self::pause_trade_prefix`].
    fn news_trade_prefix(trade_id: &str) -> String {
        format!("news:{trade_id}:")
    }

    /// Global prefix used by `snapshot()` to enumerate every active
    /// news window across every trade.
    fn news_all_prefix() -> &'static str {
        "news:"
    }

    async fn read_cooldown_index(&self) -> Result<Vec<CooldownEntry>, StateError> {
        read_index(&self.store, INDEX_COOLDOWNS_KEY).await
    }

    async fn read_seen_index(&self) -> Result<Vec<SeenEntry>, StateError> {
        read_index(&self.store, INDEX_SEEN_KEY).await
    }

    async fn read_prep_index(&self) -> Result<Vec<PrepEntry>, StateError> {
        read_index(&self.store, INDEX_PREPS_KEY).await
    }

    async fn read_veto_index(&self) -> Result<Vec<VetoEntry>, StateError> {
        read_index(&self.store, INDEX_VETOS_KEY).await
    }

    async fn write_cooldown_index(&self, entries: &[CooldownEntry]) -> Result<(), StateError> {
        write_index(&self.store, INDEX_COOLDOWNS_KEY, entries).await
    }

    async fn write_seen_index(&self, entries: &[SeenEntry]) -> Result<(), StateError> {
        write_index(&self.store, INDEX_SEEN_KEY, entries).await
    }

    async fn write_prep_index(&self, entries: &[PrepEntry]) -> Result<(), StateError> {
        write_index(&self.store, INDEX_PREPS_KEY, entries).await
    }

    async fn write_veto_index(&self, entries: &[VetoEntry]) -> Result<(), StateError> {
        write_index(&self.store, INDEX_VETOS_KEY, entries).await
    }

    async fn read_prep_block_index(&self) -> Result<Vec<PrepBlockEntry>, StateError> {
        read_index(&self.store, INDEX_PREP_BLOCKS_KEY).await
    }

    async fn write_prep_block_index(&self, entries: &[PrepBlockEntry]) -> Result<(), StateError> {
        write_index(&self.store, INDEX_PREP_BLOCKS_KEY, entries).await
    }
}

async fn read_index<T: for<'de> serde::Deserialize<'de>>(
    store: &KvStore,
    key: &str,
) -> Result<Vec<T>, StateError> {
    let raw = store
        .get(key)
        .text()
        .await
        .map_err(|e| StateError::Backend(format!("get {key}: {e:?}")))?;
    let Some(text) = raw else {
        return Ok(Vec::new());
    };
    decode_index(key, &text)
}

/// Decode a JSON-array index blob **element-wise**, dropping (and logging)
/// any single element that fails to deserialize into `T` rather than failing
/// the whole array.
///
/// Why: the index structs ([`VetoEntry`] et al.) have gained required fields
/// over time. A single legacy element written before a field existed — e.g. a
/// `VetoEntry` with no `trade_id` — would otherwise poison the strict
/// `from_str::<Vec<T>>` decode and take down *every* read-modify-write that
/// touches the index. Since `set_veto`/`set_cooldown`/… all RMW their index,
/// one bad row 500'd every veto/cooldown/prep write platform-wide (bug #6,
/// 2026-06-12). Dropping the bad element is safe: a row that won't decode
/// matched no live query anyway, and the next `write_index` rewrites the blob
/// without it (self-healing).
///
/// Only a genuinely broken *container* (not a JSON array, truncated blob) is
/// still a fatal [`StateError::Backend`] — that's not recoverable schema drift.
fn decode_index<T: for<'de> serde::Deserialize<'de>>(
    key: &str,
    text: &str,
) -> Result<Vec<T>, StateError> {
    let elements = serde_json::from_str::<Vec<serde_json::Value>>(text)
        .map_err(|e| StateError::Backend(format!("decode {key}: {e}")))?;
    let mut out = Vec::with_capacity(elements.len());
    for (idx, element) in elements.into_iter().enumerate() {
        match serde_json::from_value::<T>(element) {
            Ok(entry) => out.push(entry),
            Err(e) => warn_dropped_index_element(key, idx, &e),
        }
    }
    Ok(out)
}

/// Warn about a dropped index element. `rlog_err!` is native-safe and
/// buffers the line into the per-request R2 record.
fn warn_dropped_index_element(key: &str, idx: usize, err: &serde_json::Error) {
    rlog_err!("index decode: dropping bad element key={key} idx={idx} err={err}");
}

/// Native-safe warning for a single prefix-listed KV value that won't
/// decode. Same rationale as [`warn_dropped_index_element`], but the
/// per-key listings (`pause:…`, `news:…`) store one object per key, so
/// the key name itself identifies the dropped record — no array index.
fn warn_dropped_keyed_value(key: &str, err: &serde_json::Error) {
    rlog_err!("kv list decode: dropping bad value key={key} err={err}");
}

async fn write_index<T: serde::Serialize>(
    store: &KvStore,
    key: &str,
    entries: &[T],
) -> Result<(), StateError> {
    let body = serde_json::to_string(entries)
        .map_err(|e| StateError::Backend(format!("encode {key}: {e}")))?;
    store
        .put(key, body)
        .map_err(|e| StateError::Backend(format!("put {key} builder: {e:?}")))?
        .execute()
        .await
        .map_err(|e| StateError::Backend(format!("put {key} execute: {e:?}")))
}

impl StateStore for KvStateStore {
    async fn is_seen(&self, id: &str) -> Result<bool, StateError> {
        let key = Self::seen_key(id);
        let result = self
            .store
            .get(&key)
            .text()
            .await
            .map_err(|e| StateError::Backend(format!("get seen: {e:?}")))?;
        Ok(result.is_some())
    }

    async fn mark_seen(
        &self,
        id: &str,
        action: Action,
        seen_at: DateTime<Utc>,
        outcome: &str,
        ttl_seconds: u64,
        trade_id: Option<&str>,
    ) -> Result<(), StateError> {
        let key = Self::seen_key(id);
        let ttl = ttl_seconds.max(MIN_TTL_SECONDS);
        self.store
            .put(&key, "1")
            .map_err(|e| StateError::Backend(format!("put seen builder: {e:?}")))?
            .expiration_ttl(ttl)
            .execute()
            .await
            .map_err(|e| StateError::Backend(format!("put seen execute: {e:?}")))?;

        // Update the seen index. Best-effort; the TTL key above is the
        // authoritative replay-protection record. The index also carries
        // action / seen_at / outcome / trade_id so `status` can show
        // what happened to each id rather than just listing bare expiry
        // times.
        let expires_at = seen_at + chrono::Duration::seconds(ttl as i64);
        let mut entries = prune_expired(self.read_seen_index().await?, seen_at);
        // Drop any prior entry with the same id, then append.
        entries.retain(|e| e.id != id);
        entries.push(SeenEntry {
            id: id.to_string(),
            action,
            seen_at: Some(seen_at),
            outcome: outcome.to_string(),
            expires_at,
            trade_id: trade_id.map(str::to_string),
        });
        // Cap to the most recent N — keeps the index small.
        if entries.len() > SEEN_INDEX_CAP {
            let drop = entries.len() - SEEN_INDEX_CAP;
            entries.drain(..drop);
        }
        self.write_seen_index(&entries).await
    }

    async fn forget_seen(&self, id: &str) -> Result<(), StateError> {
        let key = Self::seen_key(id);
        // Best-effort delete: if the key already expired or was
        // never written, the index drop below is still useful.
        self.store
            .delete(&key)
            .await
            .map_err(|e| StateError::Backend(format!("delete seen: {e:?}")))?;
        let now = Utc::now();
        let mut entries = prune_expired(self.read_seen_index().await?, now);
        let before = entries.len();
        entries.retain(|e| e.id != id);
        if entries.len() != before {
            self.write_seen_index(&entries).await?;
        }
        Ok(())
    }

    async fn is_cooled_down(
        &self,
        account: Option<&str>,
        instrument: &str,
    ) -> Result<bool, StateError> {
        // Global-first: a worker-wide cooldown pauses every account.
        let global = Self::cooldown_key(None, instrument);
        if self
            .store
            .get(&global)
            .text()
            .await
            .map_err(|e| StateError::Backend(format!("get cooldown (global): {e:?}")))?
            .is_some()
        {
            return Ok(true);
        }
        if account.is_some() {
            let scoped = Self::cooldown_key(account, instrument);
            if self
                .store
                .get(&scoped)
                .text()
                .await
                .map_err(|e| StateError::Backend(format!("get cooldown (scoped): {e:?}")))?
                .is_some()
            {
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
        let key = Self::cooldown_key(account, instrument);
        let ttl = (hours as u64).saturating_mul(3600).max(MIN_TTL_SECONDS);
        self.store
            .put(&key, "1")
            .map_err(|e| StateError::Backend(format!("put cooldown builder: {e:?}")))?
            .expiration_ttl(ttl)
            .execute()
            .await
            .map_err(|e| StateError::Backend(format!("put cooldown execute: {e:?}")))?;

        let expires_at = now + chrono::Duration::seconds(ttl as i64);
        let account_owned = account.map(str::to_string);
        let mut entries = prune_expired(self.read_cooldown_index().await?, now);
        entries.retain(|e| !(e.instrument == instrument && e.account == account_owned));
        entries.push(CooldownEntry {
            instrument: instrument.to_string(),
            set_at: Some(now),
            expires_at,
            account: account_owned,
        });
        self.write_cooldown_index(&entries).await
    }

    async fn clear_cooldown(
        &self,
        account: Option<&str>,
        instrument: &str,
    ) -> Result<bool, StateError> {
        // Scoped clear — same shape as clear_veto.
        let key = Self::cooldown_key(account, instrument);
        let was = self
            .store
            .get(&key)
            .text()
            .await
            .map_err(|e| StateError::Backend(format!("get cooldown for clear: {e:?}")))?
            .is_some();
        if was {
            self.store
                .delete(&key)
                .await
                .map_err(|e| StateError::Backend(format!("delete cooldown: {e:?}")))?;
        }
        let now = Utc::now();
        let account_owned = account.map(str::to_string);
        let mut entries = prune_expired(self.read_cooldown_index().await?, now);
        let before = entries.len();
        entries.retain(|e| !(e.instrument == instrument && e.account == account_owned));
        if entries.len() != before || was {
            self.write_cooldown_index(&entries).await?;
        }
        Ok(was)
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
        let key = Self::prep_key(account, instrument, step);
        let ttl = ttl_seconds.max(MIN_TTL_SECONDS);
        // Store `<rfc3339>|<setter_id>` so the entry-time gate can read
        // the timestamp AND `clear_prep` can forget the setter's seen
        // record. See `parse_prep_value`. RFC3339 has no `|`.
        let body = format!("{}|{setter_id}", now.to_rfc3339());
        self.store
            .put(&key, body)
            .map_err(|e| StateError::Backend(format!("put prep builder: {e:?}")))?
            .expiration_ttl(ttl)
            .execute()
            .await
            .map_err(|e| StateError::Backend(format!("put prep execute: {e:?}")))?;

        let expires_at = now + chrono::Duration::seconds(ttl as i64);
        let account_owned = account.map(str::to_string);
        let mut entries = prune_expired(self.read_prep_index().await?, now);
        entries.retain(|e| {
            !(e.instrument == instrument && e.step == step && e.account == account_owned)
        });
        entries.push(PrepEntry {
            instrument: instrument.to_string(),
            step: step.to_string(),
            set_at: now,
            expires_at,
            account: account_owned,
        });
        if entries.len() > PREP_INDEX_CAP {
            let drop = entries.len() - PREP_INDEX_CAP;
            entries.drain(..drop);
        }
        self.write_prep_index(&entries).await
    }

    async fn get_prep(
        &self,
        account: Option<&str>,
        instrument: &str,
        step: &str,
    ) -> Result<Option<DateTime<Utc>>, StateError> {
        // Global-first: a worker-wide prep satisfies the gate on every
        // account. Then fall back to the account-scoped key if one
        // was supplied.
        let global = Self::prep_key(None, instrument, step);
        let mut raw = self
            .store
            .get(&global)
            .text()
            .await
            .map_err(|e| StateError::Backend(format!("get prep (global): {e:?}")))?;
        if raw.is_none() && account.is_some() {
            let scoped = Self::prep_key(account, instrument, step);
            raw = self
                .store
                .get(&scoped)
                .text()
                .await
                .map_err(|e| StateError::Backend(format!("get prep (scoped): {e:?}")))?;
        }
        let Some(text) = raw else {
            return Ok(None);
        };
        let (ts_part, _id_part) = trade_control_core::state::parse_prep_value(&text);
        let ts = DateTime::parse_from_rfc3339(ts_part)
            .map_err(|e| StateError::Backend(format!("parse prep timestamp: {e}")))?
            .with_timezone(&Utc);
        Ok(Some(ts))
    }

    async fn clear_prep(
        &self,
        account: Option<&str>,
        instrument: &str,
        step: &str,
    ) -> Result<Option<String>, StateError> {
        let key = Self::prep_key(account, instrument, step);
        let raw = self
            .store
            .get(&key)
            .text()
            .await
            .map_err(|e| StateError::Backend(format!("get prep for clear: {e:?}")))?;
        let setter_id = raw
            .as_ref()
            .map(|r| trade_control_core::state::parse_prep_value(r).1.to_string());
        let was = raw.is_some();
        if was {
            self.store
                .delete(&key)
                .await
                .map_err(|e| StateError::Backend(format!("delete prep: {e:?}")))?;
        }
        let now = Utc::now();
        let account_owned = account.map(str::to_string);
        let mut entries = prune_expired(self.read_prep_index().await?, now);
        let before = entries.len();
        entries.retain(|e| {
            !(e.instrument == instrument && e.step == step && e.account == account_owned)
        });
        if entries.len() != before || was {
            self.write_prep_index(&entries).await?;
        }
        Ok(setter_id)
    }

    async fn set_veto(
        &self,
        account: Option<&str>,
        trade_id: &str,
        instrument: &str,
        name: &str,
        ttl_seconds: u64,
    ) -> Result<(), StateError> {
        let key = Self::veto_key(account, trade_id, instrument, name);
        let ttl = ttl_seconds.max(MIN_TTL_SECONDS);
        self.store
            .put(&key, "1")
            .map_err(|e| StateError::Backend(format!("put veto builder: {e:?}")))?
            .expiration_ttl(ttl)
            .execute()
            .await
            .map_err(|e| StateError::Backend(format!("put veto execute: {e:?}")))?;

        let now = Utc::now();
        let expires_at = now + chrono::Duration::seconds(ttl as i64);
        let account_owned = account.map(str::to_string);
        let mut entries = prune_expired(self.read_veto_index().await?, now);
        entries.retain(|e| {
            !(e.trade_id == trade_id
                && e.instrument == instrument
                && e.name == name
                && e.account == account_owned)
        });
        entries.push(VetoEntry {
            trade_id: trade_id.to_string(),
            instrument: instrument.to_string(),
            name: name.to_string(),
            expires_at,
            account: account_owned,
        });
        if entries.len() > VETO_INDEX_CAP {
            let drop = entries.len() - VETO_INDEX_CAP;
            entries.drain(..drop);
        }
        self.write_veto_index(&entries).await
    }

    async fn is_vetoed(
        &self,
        account: Option<&str>,
        trade_id: &str,
        instrument: &str,
        name: &str,
    ) -> Result<bool, StateError> {
        // Global vetos affect every account; check the global key
        // first, then the account-scoped key if one was requested. Two
        // GETs in the rare-collision case; one GET on the common path
        // where there's no account-specific veto. Both keys are scoped
        // to `trade_id` so a veto from a different setup never matches.
        let global = Self::veto_key(None, trade_id, instrument, name);
        if self
            .store
            .get(&global)
            .text()
            .await
            .map_err(|e| StateError::Backend(format!("get veto (global): {e:?}")))?
            .is_some()
        {
            return Ok(true);
        }
        if account.is_some() {
            let scoped = Self::veto_key(account, trade_id, instrument, name);
            let scoped_hit = self
                .store
                .get(&scoped)
                .text()
                .await
                .map_err(|e| StateError::Backend(format!("get veto (scoped): {e:?}")))?
                .is_some();
            if scoped_hit {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn clear_veto(
        &self,
        account: Option<&str>,
        trade_id: &str,
        instrument: &str,
        name: &str,
    ) -> Result<bool, StateError> {
        // Scoped clear — clearing on one account doesn't touch a
        // different account's veto or the global veto, and clearing
        // under one trade_id doesn't touch another setup's veto.
        let key = Self::veto_key(account, trade_id, instrument, name);
        let was = self
            .store
            .get(&key)
            .text()
            .await
            .map_err(|e| StateError::Backend(format!("get veto for clear: {e:?}")))?
            .is_some();
        if was {
            self.store
                .delete(&key)
                .await
                .map_err(|e| StateError::Backend(format!("delete veto: {e:?}")))?;
        }
        let now = Utc::now();
        let account_owned = account.map(str::to_string);
        let mut entries = prune_expired(self.read_veto_index().await?, now);
        let before = entries.len();
        entries.retain(|e| {
            !(e.trade_id == trade_id
                && e.instrument == instrument
                && e.name == name
                && e.account == account_owned)
        });
        if entries.len() != before || was {
            self.write_veto_index(&entries).await?;
        }
        Ok(was)
    }

    async fn block_prep(
        &self,
        account: Option<&str>,
        instrument: &str,
        step: &str,
        now: DateTime<Utc>,
        ttl_seconds: u64,
    ) -> Result<(), StateError> {
        let key = Self::prep_block_key(account, instrument, step);
        let ttl = ttl_seconds.max(MIN_TTL_SECONDS);
        self.store
            .put(&key, "1")
            .map_err(|e| StateError::Backend(format!("put prep-block builder: {e:?}")))?
            .expiration_ttl(ttl)
            .execute()
            .await
            .map_err(|e| StateError::Backend(format!("put prep-block execute: {e:?}")))?;

        let expires_at = now + chrono::Duration::seconds(ttl as i64);
        let account_owned = account.map(str::to_string);
        let mut entries = prune_expired(self.read_prep_block_index().await?, now);
        entries.retain(|e| {
            !(e.instrument == instrument && e.step == step && e.account == account_owned)
        });
        entries.push(PrepBlockEntry {
            instrument: instrument.to_string(),
            step: step.to_string(),
            expires_at,
            account: account_owned,
        });
        if entries.len() > PREP_BLOCK_INDEX_CAP {
            let drop = entries.len() - PREP_BLOCK_INDEX_CAP;
            entries.drain(..drop);
        }
        self.write_prep_block_index(&entries).await
    }

    async fn is_prep_blocked(
        &self,
        account: Option<&str>,
        instrument: &str,
        step: &str,
    ) -> Result<bool, StateError> {
        // Global block covers every account; check the global key first,
        // then the account-scoped key if one was requested. Mirrors
        // `is_vetoed`.
        let global = Self::prep_block_key(None, instrument, step);
        if self
            .store
            .get(&global)
            .text()
            .await
            .map_err(|e| StateError::Backend(format!("get prep-block (global): {e:?}")))?
            .is_some()
        {
            return Ok(true);
        }
        if account.is_some() {
            let scoped = Self::prep_block_key(account, instrument, step);
            let scoped_hit = self
                .store
                .get(&scoped)
                .text()
                .await
                .map_err(|e| StateError::Backend(format!("get prep-block (scoped): {e:?}")))?
                .is_some();
            if scoped_hit {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn clear_prep_block(
        &self,
        account: Option<&str>,
        instrument: &str,
        step: &str,
    ) -> Result<bool, StateError> {
        let key = Self::prep_block_key(account, instrument, step);
        let was = self
            .store
            .get(&key)
            .text()
            .await
            .map_err(|e| StateError::Backend(format!("get prep-block for clear: {e:?}")))?
            .is_some();
        if was {
            self.store
                .delete(&key)
                .await
                .map_err(|e| StateError::Backend(format!("delete prep-block: {e:?}")))?;
        }
        let now = Utc::now();
        let account_owned = account.map(str::to_string);
        let mut entries = prune_expired(self.read_prep_block_index().await?, now);
        let before = entries.len();
        entries.retain(|e| {
            !(e.instrument == instrument && e.step == step && e.account == account_owned)
        });
        if entries.len() != before || was {
            self.write_prep_block_index(&entries).await?;
        }
        Ok(was)
    }

    async fn snapshot(&self) -> Result<Snapshot, StateError> {
        let now: DateTime<Utc> = Utc::now();
        let cooldowns = prune_expired(self.read_cooldown_index().await?, now);
        let recent_seen = prune_expired(self.read_seen_index().await?, now);
        let preps = prune_expired(self.read_prep_index().await?, now);
        let vetos = prune_expired(self.read_veto_index().await?, now);
        let prep_blocks = prune_expired(self.read_prep_block_index().await?, now);
        // Pauses don't use an index — there's no per-(account,instrument)
        // listing surface that makes one natural — so the snapshot does a
        // `kv.list(prefix="pause:")` scan instead. KV's TTL on each pause
        // entry handles automatic expiry; the explicit prune below is
        // defensive against a list/get race where a key is reported live
        // but its TTL has just lapsed.
        let pauses = prune_expired(
            list_pauses_with_prefix(&self.store, Self::pause_all_prefix()).await?,
            now,
        );
        // News windows live in a separate KV namespace from pauses (see
        // NewsEntry); the snapshot scans them with their own prefix so
        // the operator can see why a reversal-close might fire.
        let news_windows = prune_expired(
            list_news_with_prefix(&self.store, Self::news_all_prefix()).await?,
            now,
        );
        // Per-trade spread-blackout records + the singleton window
        // marker. Same `kv.list` scan as pauses/news; the `rec:` prefix
        // keeps the record scan disjoint from the window key.
        let spread_blackouts = prune_expired(
            list_spread_blackouts_with_prefix(&self.store, Self::spread_blackout_record_prefix())
                .await?,
            now,
        );
        let spread_blackout_window = self
            .get_spread_blackout_window()
            .await?
            .filter(|w| w.expires_at > now);
        Ok(Snapshot {
            now,
            cooldowns,
            recent_seen,
            preps,
            vetos,
            pauses,
            news_windows,
            prep_blocks,
            spread_blackouts,
            spread_blackout_window,
        })
    }

    async fn record_entry_attempt(&self, attempt: EntryAttempt) -> Result<(), StateError> {
        let key = Self::entry_attempt_key(
            attempt.account.as_deref(),
            &attempt.trade_id,
            attempt.attempt_no,
        );
        // TTL the row to `expires_at` so dead attempts age out of KV
        // (and out of `list_entry_attempts`) without explicit cleanup.
        let now = Utc::now();
        let ttl = (attempt.expires_at - now)
            .num_seconds()
            .max(MIN_TTL_SECONDS as i64) as u64;
        let body = serde_json::to_string(&attempt)
            .map_err(|e| StateError::Backend(format!("encode entry_attempt: {e}")))?;
        self.store
            .put(&key, body)
            .map_err(|e| StateError::Backend(format!("put entry_attempt builder: {e:?}")))?
            .expiration_ttl(ttl)
            .execute()
            .await
            .map_err(|e| StateError::Backend(format!("put entry_attempt execute: {e:?}")))?;
        Ok(())
    }

    async fn list_entry_attempts(
        &self,
        account: Option<&str>,
        trade_id: &str,
    ) -> Result<Vec<EntryAttempt>, StateError> {
        let prefix = Self::entry_attempt_prefix(account, trade_id);
        // Page through `kv.list` until `list_complete` so we don't miss
        // attempts above the default page size. Worker KV pages are
        // 1000 keys by default — well above any sane `max_retries` —
        // but pagination is cheap and future-proofs the gate.
        let mut keys: Vec<String> = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut builder = self.store.list().prefix(prefix.clone());
            if let Some(c) = cursor.take() {
                builder = builder.cursor(c);
            }
            let resp = builder
                .execute()
                .await
                .map_err(|e| StateError::Backend(format!("list entry_attempt: {e:?}")))?;
            keys.extend(resp.keys.into_iter().map(|k| k.name));
            if resp.list_complete {
                break;
            }
            match resp.cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }

        let mut attempts: Vec<EntryAttempt> = Vec::with_capacity(keys.len());
        for key in keys {
            let raw = self
                .store
                .get(&key)
                .text()
                .await
                .map_err(|e| StateError::Backend(format!("get {key}: {e:?}")))?;
            // A key listed but with no value means it expired between
            // list and get — skip silently.
            let Some(text) = raw else { continue };
            let attempt: EntryAttempt = serde_json::from_str(&text)
                .map_err(|e| StateError::Backend(format!("decode {key}: {e}")))?;
            attempts.push(attempt);
        }
        attempts.sort_by_key(|a| a.attempt_no);
        Ok(attempts)
    }

    async fn list_all_entry_attempts(&self) -> Result<Vec<EntryAttempt>, StateError> {
        // Walk every `entry_attempt:` row regardless of scope or
        // trade_id — the scheduled sweep needs a global view. Same
        // pagination shape as `list_entry_attempts`.
        let prefix = "entry_attempt:";
        let mut keys: Vec<String> = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut builder = self.store.list().prefix(prefix.to_string());
            if let Some(c) = cursor.take() {
                builder = builder.cursor(c);
            }
            let resp = builder
                .execute()
                .await
                .map_err(|e| StateError::Backend(format!("list entry_attempt (all): {e:?}")))?;
            keys.extend(resp.keys.into_iter().map(|k| k.name));
            if resp.list_complete {
                break;
            }
            match resp.cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }

        let mut attempts: Vec<EntryAttempt> = Vec::with_capacity(keys.len());
        for key in keys {
            let raw = self
                .store
                .get(&key)
                .text()
                .await
                .map_err(|e| StateError::Backend(format!("get {key}: {e:?}")))?;
            let Some(text) = raw else { continue };
            let attempt: EntryAttempt = serde_json::from_str(&text)
                .map_err(|e| StateError::Backend(format!("decode {key}: {e}")))?;
            attempts.push(attempt);
        }
        Ok(attempts)
    }

    async fn delete_entry_attempt(
        &self,
        account: Option<&str>,
        trade_id: &str,
        attempt_no: u32,
    ) -> Result<(), StateError> {
        let key = Self::entry_attempt_key(account, trade_id, attempt_no);
        // Best-effort: KV delete is a no-op if the key is already gone.
        self.store
            .delete(&key)
            .await
            .map_err(|e| StateError::Backend(format!("delete entry_attempt: {e:?}")))?;
        Ok(())
    }

    async fn set_entry_attempt_broker_trade_id(
        &self,
        account: Option<&str>,
        trade_id: &str,
        attempt_no: u32,
        broker_trade_id: &str,
    ) -> Result<(), StateError> {
        let key = Self::entry_attempt_key(account, trade_id, attempt_no);
        let raw = self
            .store
            .get(&key)
            .text()
            .await
            .map_err(|e| StateError::Backend(format!("get {key}: {e:?}")))?;
        let Some(text) = raw else {
            return Err(StateError::Backend(format!(
                "entry_attempt missing for {key} (expired or never recorded)"
            )));
        };
        let mut attempt: EntryAttempt = serde_json::from_str(&text)
            .map_err(|e| StateError::Backend(format!("decode {key}: {e}")))?;
        attempt.broker_trade_id = Some(broker_trade_id.to_string());
        // Preserve the row's remaining lifetime: re-derive TTL from
        // `expires_at` (clamped to KV's minimum) rather than letting
        // the rewrite reset to "forever".
        let now = Utc::now();
        let ttl = (attempt.expires_at - now)
            .num_seconds()
            .max(MIN_TTL_SECONDS as i64) as u64;
        let body = serde_json::to_string(&attempt)
            .map_err(|e| StateError::Backend(format!("encode {key}: {e}")))?;
        self.store
            .put(&key, body)
            .map_err(|e| StateError::Backend(format!("put {key} builder: {e:?}")))?
            .expiration_ttl(ttl)
            .execute()
            .await
            .map_err(|e| StateError::Backend(format!("put {key} execute: {e:?}")))?;
        Ok(())
    }

    async fn is_retry_fire_seen(
        &self,
        account: Option<&str>,
        trade_id: &str,
        shell_time: DateTime<Utc>,
    ) -> Result<bool, StateError> {
        let key = Self::retry_fire_seen_key(account, trade_id, shell_time);
        let raw = self
            .store
            .get(&key)
            .text()
            .await
            .map_err(|e| StateError::Backend(format!("get seen-retry: {e:?}")))?;
        Ok(raw.is_some())
    }

    async fn mark_retry_fire_seen(
        &self,
        account: Option<&str>,
        trade_id: &str,
        shell_time: DateTime<Utc>,
        ttl_seconds: u64,
    ) -> Result<(), StateError> {
        let key = Self::retry_fire_seen_key(account, trade_id, shell_time);
        let ttl = ttl_seconds.max(MIN_TTL_SECONDS);
        self.store
            .put(&key, "1")
            .map_err(|e| StateError::Backend(format!("put seen-retry builder: {e:?}")))?
            .expiration_ttl(ttl)
            .execute()
            .await
            .map_err(|e| StateError::Backend(format!("put seen-retry execute: {e:?}")))?;
        Ok(())
    }

    async fn set_pause(
        &self,
        trade_id: &str,
        blackout_id: &str,
        reason: Option<&str>,
        now: DateTime<Utc>,
        ttl_seconds: u64,
    ) -> Result<(), StateError> {
        let key = Self::pause_key(trade_id, blackout_id);
        let ttl = ttl_seconds.max(MIN_TTL_SECONDS);
        let entry = PauseEntry {
            trade_id: trade_id.to_string(),
            blackout_id: blackout_id.to_string(),
            reason: reason.map(str::to_string),
            set_at: now,
            expires_at: now + chrono::Duration::seconds(ttl as i64),
        };
        let body = serde_json::to_string(&entry)
            .map_err(|e| StateError::Backend(format!("encode pause: {e}")))?;
        self.store
            .put(&key, body)
            .map_err(|e| StateError::Backend(format!("put pause builder: {e:?}")))?
            .expiration_ttl(ttl)
            .execute()
            .await
            .map_err(|e| StateError::Backend(format!("put pause execute: {e:?}")))?;
        Ok(())
    }

    async fn list_pauses_for_trade(&self, trade_id: &str) -> Result<Vec<PauseEntry>, StateError> {
        list_pauses_with_prefix(&self.store, &Self::pause_trade_prefix(trade_id)).await
    }

    async fn clear_pause(&self, trade_id: &str, blackout_id: &str) -> Result<bool, StateError> {
        let key = Self::pause_key(trade_id, blackout_id);
        // Check presence first so we can return whether the clear was a
        // no-op (operator visibility). Matches the shape of clear_veto /
        // clear_cooldown rather than the cheaper unconditional delete.
        let was = self
            .store
            .get(&key)
            .text()
            .await
            .map_err(|e| StateError::Backend(format!("get pause for clear: {e:?}")))?
            .is_some();
        if was {
            self.store
                .delete(&key)
                .await
                .map_err(|e| StateError::Backend(format!("delete pause: {e:?}")))?;
        }
        Ok(was)
    }

    async fn set_news_window(
        &self,
        trade_id: &str,
        news_id: &str,
        reason: Option<&str>,
        now: DateTime<Utc>,
        ttl_seconds: u64,
    ) -> Result<(), StateError> {
        let key = Self::news_key(trade_id, news_id);
        let ttl = ttl_seconds.max(MIN_TTL_SECONDS);
        let entry = NewsEntry {
            trade_id: trade_id.to_string(),
            news_id: news_id.to_string(),
            reason: reason.map(str::to_string),
            set_at: now,
            expires_at: now + chrono::Duration::seconds(ttl as i64),
        };
        let body = serde_json::to_string(&entry)
            .map_err(|e| StateError::Backend(format!("encode news: {e}")))?;
        self.store
            .put(&key, body)
            .map_err(|e| StateError::Backend(format!("put news builder: {e:?}")))?
            .expiration_ttl(ttl)
            .execute()
            .await
            .map_err(|e| StateError::Backend(format!("put news execute: {e:?}")))?;
        Ok(())
    }

    async fn list_news_windows_for_trade(
        &self,
        trade_id: &str,
    ) -> Result<Vec<NewsEntry>, StateError> {
        list_news_with_prefix(&self.store, &Self::news_trade_prefix(trade_id)).await
    }

    async fn clear_news_window(&self, trade_id: &str, news_id: &str) -> Result<bool, StateError> {
        let key = Self::news_key(trade_id, news_id);
        let was = self
            .store
            .get(&key)
            .text()
            .await
            .map_err(|e| StateError::Backend(format!("get news for clear: {e:?}")))?
            .is_some();
        if was {
            self.store
                .delete(&key)
                .await
                .map_err(|e| StateError::Backend(format!("delete news: {e:?}")))?;
        }
        Ok(was)
    }

    async fn set_spread_blackout_window(
        &self,
        now: DateTime<Utc>,
        ttl_seconds: u64,
    ) -> Result<(), StateError> {
        let key = Self::spread_blackout_window_key();
        let ttl = ttl_seconds.max(MIN_TTL_SECONDS);
        let entry = SpreadBlackoutWindow {
            opened_at: now,
            expires_at: now + chrono::Duration::seconds(ttl as i64),
        };
        let body = serde_json::to_string(&entry)
            .map_err(|e| StateError::Backend(format!("encode spread-blackout window: {e}")))?;
        self.store
            .put(key, body)
            .map_err(|e| StateError::Backend(format!("put spread-blackout window builder: {e:?}")))?
            .expiration_ttl(ttl)
            .execute()
            .await
            .map_err(|e| {
                StateError::Backend(format!("put spread-blackout window execute: {e:?}"))
            })?;
        Ok(())
    }

    async fn get_spread_blackout_window(&self) -> Result<Option<SpreadBlackoutWindow>, StateError> {
        let key = Self::spread_blackout_window_key();
        let raw = self
            .store
            .get(key)
            .text()
            .await
            .map_err(|e| StateError::Backend(format!("get spread-blackout window: {e:?}")))?;
        let Some(text) = raw else { return Ok(None) };
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| StateError::Backend(format!("decode spread-blackout window: {e}")))
    }

    async fn upsert_spread_blackout_record(
        &self,
        record: &SpreadBlackoutRecord,
        ttl_seconds: u64,
    ) -> Result<(), StateError> {
        let key = Self::spread_blackout_record_key(&record.trade_id);
        let ttl = ttl_seconds.max(MIN_TTL_SECONDS);
        let body = serde_json::to_string(record)
            .map_err(|e| StateError::Backend(format!("encode spread-blackout record: {e}")))?;
        self.store
            .put(&key, body)
            .map_err(|e| StateError::Backend(format!("put {key} builder: {e:?}")))?
            .expiration_ttl(ttl)
            .execute()
            .await
            .map_err(|e| StateError::Backend(format!("put {key} execute: {e:?}")))?;
        Ok(())
    }

    async fn get_spread_blackout_record(
        &self,
        trade_id: &str,
    ) -> Result<Option<SpreadBlackoutRecord>, StateError> {
        let key = Self::spread_blackout_record_key(trade_id);
        let raw = self
            .store
            .get(&key)
            .text()
            .await
            .map_err(|e| StateError::Backend(format!("get {key}: {e:?}")))?;
        let Some(text) = raw else { return Ok(None) };
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| StateError::Backend(format!("decode {key}: {e}")))
    }

    async fn list_all_spread_blackout_records(
        &self,
    ) -> Result<Vec<SpreadBlackoutRecord>, StateError> {
        list_spread_blackouts_with_prefix(&self.store, Self::spread_blackout_record_prefix()).await
    }

    async fn clear_spread_blackout_record(&self, trade_id: &str) -> Result<(), StateError> {
        let key = Self::spread_blackout_record_key(trade_id);
        // Best-effort: KV delete is a no-op if the key is already gone.
        self.store
            .delete(&key)
            .await
            .map_err(|e| StateError::Backend(format!("delete {key}: {e:?}")))?;
        Ok(())
    }

    async fn get_mw_state(
        &self,
        account: Option<&str>,
        trade_id: &str,
    ) -> Result<Option<MwState>, StateError> {
        // Global-first: a global row satisfies an account-scoped query,
        // mirroring `is_vetoed` / `get_prep`.
        let global = Self::mw_state_key(None, trade_id);
        let mut raw = self
            .store
            .get(&global)
            .text()
            .await
            .map_err(|e| StateError::Backend(format!("get mw-state (global): {e:?}")))?;
        if raw.is_none() && account.is_some() {
            let scoped = Self::mw_state_key(account, trade_id);
            raw = self
                .store
                .get(&scoped)
                .text()
                .await
                .map_err(|e| StateError::Backend(format!("get mw-state (scoped): {e:?}")))?;
        }
        let Some(text) = raw else { return Ok(None) };
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| StateError::Backend(format!("decode mw-state: {e}")))
    }

    async fn upsert_mw_state(
        &self,
        account: Option<&str>,
        trade_id: &str,
        state: &MwState,
        ttl_seconds: u64,
    ) -> Result<(), StateError> {
        let key = Self::mw_state_key(account, trade_id);
        let ttl = ttl_seconds.max(MIN_TTL_SECONDS);
        let body = serde_json::to_string(state)
            .map_err(|e| StateError::Backend(format!("encode mw-state: {e}")))?;
        self.store
            .put(&key, body)
            .map_err(|e| StateError::Backend(format!("put {key} builder: {e:?}")))?
            .expiration_ttl(ttl)
            .execute()
            .await
            .map_err(|e| StateError::Backend(format!("put {key} execute: {e:?}")))?;
        Ok(())
    }

    async fn clear_mw_state(
        &self,
        account: Option<&str>,
        trade_id: &str,
    ) -> Result<(), StateError> {
        let key = Self::mw_state_key(account, trade_id);
        // Best-effort: KV delete is a no-op if the key is already gone.
        self.store
            .delete(&key)
            .await
            .map_err(|e| StateError::Backend(format!("delete {key}: {e:?}")))?;
        Ok(())
    }

    async fn put_trade_plan(
        &self,
        account: Option<&str>,
        plan: &TradePlan,
        ttl_seconds: u64,
    ) -> Result<(), StateError> {
        let key = Self::trade_plan_key(account, &plan.trade_id);
        let ttl = ttl_seconds.max(MIN_TTL_SECONDS);
        let body = serde_json::to_string(plan)
            .map_err(|e| StateError::Backend(format!("encode plan: {e}")))?;
        self.store
            .put(&key, body)
            .map_err(|e| StateError::Backend(format!("put {key} builder: {e:?}")))?
            .expiration_ttl(ttl)
            .execute()
            .await
            .map_err(|e| StateError::Backend(format!("put {key} execute: {e:?}")))?;
        Ok(())
    }

    async fn get_trade_plan(
        &self,
        account: Option<&str>,
        trade_id: &str,
    ) -> Result<Option<TradePlan>, StateError> {
        let key = Self::trade_plan_key(account, trade_id);
        let raw = self
            .store
            .get(&key)
            .text()
            .await
            .map_err(|e| StateError::Backend(format!("get {key}: {e:?}")))?;
        let Some(text) = raw else { return Ok(None) };
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| StateError::Backend(format!("decode plan: {e}")))
    }

    async fn list_all_trade_plans(&self) -> Result<Vec<StoredPlan>, StateError> {
        // Walk every `plan:` row across scopes — the engine needs a global
        // view each tick. Same pagination shape as `list_all_entry_attempts`;
        // we keep the KEYS (not just values) so the account scope can be
        // recovered from the `plan:{scope}:{trade_id}` segment.
        let prefix = "plan:";
        let mut keys: Vec<String> = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut builder = self.store.list().prefix(prefix.to_string());
            if let Some(c) = cursor.take() {
                builder = builder.cursor(c);
            }
            let resp = builder
                .execute()
                .await
                .map_err(|e| StateError::Backend(format!("list plan (all): {e:?}")))?;
            keys.extend(resp.keys.into_iter().map(|k| k.name));
            if resp.list_complete {
                break;
            }
            match resp.cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }

        let mut plans: Vec<StoredPlan> = Vec::with_capacity(keys.len());
        for key in keys {
            // `plan:{scope}:{trade_id}` — the scope is the segment between the
            // first and second colon.
            let Some(rest) = key.strip_prefix("plan:") else {
                continue;
            };
            let Some((scope, _trade_id)) = rest.split_once(':') else {
                continue;
            };
            let raw = self
                .store
                .get(&key)
                .text()
                .await
                .map_err(|e| StateError::Backend(format!("get {key}: {e:?}")))?;
            let Some(text) = raw else { continue };
            let plan: TradePlan = serde_json::from_str(&text)
                .map_err(|e| StateError::Backend(format!("decode {key}: {e}")))?;
            plans.push(StoredPlan {
                account: account_from_scope(scope),
                plan,
            });
        }
        Ok(plans)
    }

    async fn clear_trade_plan(
        &self,
        account: Option<&str>,
        trade_id: &str,
    ) -> Result<(), StateError> {
        let key = Self::trade_plan_key(account, trade_id);
        self.store
            .delete(&key)
            .await
            .map_err(|e| StateError::Backend(format!("delete {key}: {e:?}")))?;
        Ok(())
    }

    async fn get_plan_state(
        &self,
        account: Option<&str>,
        trade_id: &str,
    ) -> Result<Option<PlanState>, StateError> {
        let key = Self::plan_state_key(account, trade_id);
        let raw = self
            .store
            .get(&key)
            .text()
            .await
            .map_err(|e| StateError::Backend(format!("get {key}: {e:?}")))?;
        let Some(text) = raw else { return Ok(None) };
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| StateError::Backend(format!("decode plan-state: {e}")))
    }

    async fn put_plan_state(
        &self,
        account: Option<&str>,
        trade_id: &str,
        state: &PlanState,
        ttl_seconds: u64,
    ) -> Result<(), StateError> {
        let key = Self::plan_state_key(account, trade_id);
        let ttl = ttl_seconds.max(MIN_TTL_SECONDS);
        let body = serde_json::to_string(state)
            .map_err(|e| StateError::Backend(format!("encode plan-state: {e}")))?;
        self.store
            .put(&key, body)
            .map_err(|e| StateError::Backend(format!("put {key} builder: {e:?}")))?
            .expiration_ttl(ttl)
            .execute()
            .await
            .map_err(|e| StateError::Backend(format!("put {key} execute: {e:?}")))?;
        Ok(())
    }

    async fn clear_plan_state(
        &self,
        account: Option<&str>,
        trade_id: &str,
    ) -> Result<(), StateError> {
        let key = Self::plan_state_key(account, trade_id);
        self.store
            .delete(&key)
            .await
            .map_err(|e| StateError::Backend(format!("delete {key}: {e:?}")))?;
        Ok(())
    }
}

/// Page through `kv.list` with `prefix`, decoding each value as a
/// `PauseEntry`. Shared between [`KvStateStore::list_pauses_for_trade`]
/// (per-trade prefix) and the `pauses:` section of `snapshot`
/// (global `pause:` prefix). The pagination loop mirrors
/// `list_entry_attempts` — KV's default page is 1000 keys, well above
/// any realistic blackout count, but the loop future-proofs the call.
async fn list_pauses_with_prefix(
    store: &KvStore,
    prefix: &str,
) -> Result<Vec<PauseEntry>, StateError> {
    list_json_with_prefix(store, prefix, "pause").await
}

/// Same shape as [`list_pauses_with_prefix`], for news windows.
async fn list_news_with_prefix(
    store: &KvStore,
    prefix: &str,
) -> Result<Vec<NewsEntry>, StateError> {
    list_json_with_prefix(store, prefix, "news").await
}

/// Same shape as [`list_pauses_with_prefix`], for the recovery watcher's
/// per-trade spread-blackout records.
async fn list_spread_blackouts_with_prefix(
    store: &KvStore,
    prefix: &str,
) -> Result<Vec<SpreadBlackoutRecord>, StateError> {
    list_json_with_prefix(store, prefix, "spread-blackout").await
}

/// Generic prefix-paginated JSON list — page through `kv.list`,
/// decode each value as `T`. `kind_label` rides on error messages so
/// "list pause: ..." vs "list news: ..." still tell the operator
/// which namespace failed.
async fn list_json_with_prefix<T: for<'de> serde::Deserialize<'de>>(
    store: &KvStore,
    prefix: &str,
    kind_label: &'static str,
) -> Result<Vec<T>, StateError> {
    let mut keys: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut builder = store.list().prefix(prefix.to_string());
        if let Some(c) = cursor.take() {
            builder = builder.cursor(c);
        }
        let resp = builder
            .execute()
            .await
            .map_err(|e| StateError::Backend(format!("list {kind_label}: {e:?}")))?;
        keys.extend(resp.keys.into_iter().map(|k| k.name));
        if resp.list_complete {
            break;
        }
        match resp.cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    let mut out: Vec<T> = Vec::with_capacity(keys.len());
    for key in keys {
        // A KV I/O failure is a genuine backend error — stays fatal.
        let raw = store
            .get(&key)
            .text()
            .await
            .map_err(|e| StateError::Backend(format!("get {key}: {e:?}")))?;
        let Some(text) = raw else { continue };
        // Schema drift on one key (a legacy value missing a since-added
        // required field) must not sink the whole listing — drop it with a
        // warning, same policy as `read_index`. Bug #6 was the array-blob
        // version of this; this is the per-key version.
        if let Some(entry) = decode_keyed_value::<T>(&key, &text) {
            out.push(entry);
        }
    }
    Ok(out)
}

/// Decode one prefix-listed KV value, returning `None` (and logging a
/// warning) if it won't deserialize into `T`. The drop-not-fatal twin of
/// the per-element handling in [`decode_index`], for the one-object-per-key
/// listings (`pause:…`, `news:…`). Pure, so it's unit-testable without a
/// `KvStore`.
fn decode_keyed_value<T: for<'de> serde::Deserialize<'de>>(key: &str, text: &str) -> Option<T> {
    match serde_json::from_str::<T>(text) {
        Ok(entry) => Some(entry),
        Err(e) => {
            warn_dropped_keyed_value(key, &e);
            None
        }
    }
}

#[cfg(test)]
mod decode_index_tests {
    use super::*;

    // A valid veto element serialized to the on-disk shape.
    const GOOD_VETO: &str = r#"{
        "trade_id": "m-eur-usd-57f5b5a7",
        "instrument": "EUR_USD",
        "name": "mw-abort",
        "expires_at": "2026-06-12T08:00:00Z",
        "account": "reversals"
    }"#;

    // The legacy element from bug #6: no `trade_id` field at all.
    const LEGACY_VETO_NO_TRADE_ID: &str = r#"{
        "instrument": "EUR_USD",
        "name": "too-high",
        "expires_at": "2026-06-12T08:00:00Z",
        "account": null
    }"#;

    /// The whole incident: a `trade_id`-less legacy veto must be dropped, not
    /// fatal, and the good entry survives.
    #[test]
    fn drops_legacy_element_keeps_good_one() {
        let blob = format!("[{LEGACY_VETO_NO_TRADE_ID},{GOOD_VETO}]");
        let entries: Vec<VetoEntry> =
            decode_index("index:vetos", &blob).expect("element drift must not be fatal");
        assert_eq!(entries.len(), 1, "only the good veto should survive");
        assert_eq!(entries[0].trade_id, "m-eur-usd-57f5b5a7");
        assert_eq!(entries[0].name, "mw-abort");
    }

    /// An all-good blob round-trips unchanged.
    #[test]
    fn keeps_all_valid_elements() {
        let blob = format!("[{GOOD_VETO},{GOOD_VETO}]");
        let entries: Vec<VetoEntry> = decode_index("index:vetos", &blob).expect("valid blob");
        assert_eq!(entries.len(), 2);
    }

    /// An empty array decodes to an empty vec (read of a never-written index
    /// goes through the missing-key path, but an explicit `[]` must also work).
    #[test]
    fn empty_array_is_empty() {
        let entries: Vec<VetoEntry> = decode_index("index:vetos", "[]").expect("empty array");
        assert!(entries.is_empty());
    }

    /// A genuinely broken *container* (not a JSON array) is still fatal — we
    /// only tolerate element-level drift, not a corrupt blob.
    #[test]
    fn non_array_container_is_fatal() {
        let err = decode_index::<VetoEntry>("index:vetos", "{").unwrap_err();
        assert!(matches!(err, StateError::Backend(_)), "got {err:?}");
        let err = decode_index::<VetoEntry>("index:vetos", "not json").unwrap_err();
        assert!(matches!(err, StateError::Backend(_)), "got {err:?}");
    }

    /// The hardening is generic: the same drop-not-fatal behaviour covers
    /// every index struct. A `PrepEntry` missing its required `step` is
    /// dropped while a valid sibling survives.
    #[test]
    fn generic_over_other_index_structs() {
        let good_prep = r#"{
            "instrument": "EUR_USD",
            "step": "break-and-close",
            "set_at": "2026-06-12T07:00:00Z",
            "expires_at": "2026-06-12T08:00:00Z",
            "account": null
        }"#;
        let bad_prep = r#"{
            "instrument": "EUR_USD",
            "set_at": "2026-06-12T07:00:00Z",
            "expires_at": "2026-06-12T08:00:00Z",
            "account": null
        }"#;
        let blob = format!("[{bad_prep},{good_prep}]");
        let entries: Vec<PrepEntry> = decode_index("index:preps", &blob).expect("not fatal");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].step, "break-and-close");
    }

    // --- per-key prefix-listing decode (pause:/news: keys) ---

    const GOOD_PAUSE: &str = r#"{
        "trade_id": "m-eur-usd-57f5b5a7",
        "blackout_id": "nfp",
        "set_at": "2026-06-12T07:00:00Z",
        "expires_at": "2026-06-12T08:00:00Z"
    }"#;

    /// A valid prefix-listed value decodes.
    #[test]
    fn keyed_value_decodes_good() {
        let entry: Option<PauseEntry> =
            decode_keyed_value("pause:m-eur-usd-57f5b5a7:nfp", GOOD_PAUSE);
        let entry = entry.expect("valid pause value");
        assert_eq!(entry.blackout_id, "nfp");
    }

    /// A legacy value missing a since-added required field is dropped
    /// (None + warning), not fatal — so one bad key can't sink the whole
    /// `pause:`/`news:` listing (the per-key cousin of bug #6).
    #[test]
    fn keyed_value_drops_legacy_missing_field() {
        let legacy_no_blackout = r#"{
            "trade_id": "m-eur-usd-57f5b5a7",
            "set_at": "2026-06-12T07:00:00Z",
            "expires_at": "2026-06-12T08:00:00Z"
        }"#;
        let entry: Option<PauseEntry> =
            decode_keyed_value("pause:m-eur-usd-57f5b5a7:nfp", legacy_no_blackout);
        assert!(
            entry.is_none(),
            "a value missing blackout_id must be dropped"
        );
    }

    /// Malformed JSON for a single key is also dropped, not propagated —
    /// the listing keeps going.
    #[test]
    fn keyed_value_drops_malformed_json() {
        let entry: Option<NewsEntry> = decode_keyed_value("news:t:x", "{not json");
        assert!(entry.is_none());
    }

    // --- spread-blackout window + per-trade record decode ---

    const GOOD_BLACKOUT_WINDOW: &str = r#"{
        "opened_at": "2026-03-12T21:05:00Z",
        "expires_at": "2026-03-13T00:05:00Z"
    }"#;

    /// A full per-trade record (apply-side payload populated, as
    /// Sub-plans 4/5 will write it) decodes off a prefix-listed key.
    const GOOD_BLACKOUT_RECORD: &str = r#"{
        "trade_id": "hs-eur-nzd-c1e0f25b",
        "instrument": "EUR_NZD",
        "account": "reversals",
        "applied": true,
        "opened_at": "2026-03-12T21:05:00Z",
        "expires_at": "2026-03-13T00:05:00Z",
        "original_stops": [{"position_or_order_id": "pos-1", "original_stop": 1.8234}],
        "cancelled_orders": []
    }"#;

    /// The window marker decodes (singleton-key value path).
    #[test]
    fn blackout_window_decodes_good() {
        let entry: Option<SpreadBlackoutWindow> =
            decode_keyed_value("spread-blackout:window", GOOD_BLACKOUT_WINDOW);
        let entry = entry.expect("valid window value");
        assert_eq!(entry.opened_at.to_rfc3339(), "2026-03-12T21:05:00+00:00");
    }

    /// A full per-trade record decodes off the prefix-listed key path.
    #[test]
    fn blackout_record_decodes_good() {
        let entry: Option<SpreadBlackoutRecord> = decode_keyed_value(
            "spread-blackout:rec:hs-eur-nzd-c1e0f25b",
            GOOD_BLACKOUT_RECORD,
        );
        let entry = entry.expect("valid record value");
        assert_eq!(entry.trade_id, "hs-eur-nzd-c1e0f25b");
        assert!(entry.applied);
        assert_eq!(entry.account.as_deref(), Some("reversals"));
        assert_eq!(entry.original_stops.len(), 1);
    }

    /// A Sub-plan-2-era record (no apply-side payload) decodes with the
    /// reserved fields defaulting to empty + `account` defaulting None.
    #[test]
    fn blackout_record_decodes_minimal_with_defaults() {
        let minimal = r#"{
            "trade_id": "hs-eur-nzd-c1e0f25b",
            "instrument": "EUR_NZD",
            "applied": false,
            "opened_at": "2026-03-12T21:05:00Z",
            "expires_at": "2026-03-13T00:05:00Z"
        }"#;
        let entry: Option<SpreadBlackoutRecord> =
            decode_keyed_value("spread-blackout:rec:hs-eur-nzd-c1e0f25b", minimal);
        let entry = entry.expect("minimal record decodes");
        assert!(!entry.applied);
        assert_eq!(entry.account, None);
        assert!(entry.original_stops.is_empty());
        assert!(entry.cancelled_orders.is_empty());
    }

    /// A record missing a required field (`applied`) is dropped, not
    /// fatal — one bad row never sinks the watcher's whole listing.
    #[test]
    fn blackout_record_drops_when_missing_required_field() {
        let bad = r#"{
            "trade_id": "hs-eur-nzd-c1e0f25b",
            "instrument": "EUR_NZD",
            "opened_at": "2026-03-12T21:05:00Z",
            "expires_at": "2026-03-13T00:05:00Z"
        }"#;
        let entry: Option<SpreadBlackoutRecord> =
            decode_keyed_value("spread-blackout:rec:hs-eur-nzd-c1e0f25b", bad);
        assert!(
            entry.is_none(),
            "a record missing `applied` must be dropped"
        );
    }
}
