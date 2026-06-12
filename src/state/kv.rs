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
use trade_control_core::state::{
    CooldownEntry, EntryAttempt, MIN_TTL_SECONDS, NewsEntry, PREP_BLOCK_INDEX_CAP, PREP_INDEX_CAP,
    PauseEntry, PrepBlockEntry, PrepEntry, SEEN_INDEX_CAP, SeenEntry, Snapshot, StateError,
    StateStore, VETO_INDEX_CAP, VetoEntry, account_scope, prune_expired,
};
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

/// Native-safe warning for a dropped index element. Mirrors the
/// `log_skip` shim in `src/lib.rs`: `worker::console_log!` panics off-wasm
/// (it calls into `web_sys`), so emit via `tracing::warn!` under `cargo test`
/// and the real worker console on wasm.
fn warn_dropped_index_element(key: &str, idx: usize, err: &serde_json::Error) {
    #[cfg(target_arch = "wasm32")]
    worker::console_log!(
        "index decode: dropping bad element key={} idx={} err={}",
        key,
        idx,
        err,
    );
    #[cfg(not(target_arch = "wasm32"))]
    tracing::warn!("index decode: dropping bad element key={key} idx={idx} err={err}");
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
        Ok(Snapshot {
            now,
            cooldowns,
            recent_seen,
            preps,
            vetos,
            pauses,
            news_windows,
            prep_blocks,
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
        let raw = store
            .get(&key)
            .text()
            .await
            .map_err(|e| StateError::Backend(format!("get {key}: {e:?}")))?;
        let Some(text) = raw else { continue };
        let entry: T = serde_json::from_str(&text)
            .map_err(|e| StateError::Backend(format!("decode {key}: {e}")))?;
        out.push(entry);
    }
    Ok(out)
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
}
