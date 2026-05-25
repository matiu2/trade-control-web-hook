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
    CooldownEntry, EntryAttempt, MIN_TTL_SECONDS, PREP_INDEX_CAP, PrepEntry, SEEN_INDEX_CAP,
    SeenEntry, Snapshot, StateError, StateStore, VETO_INDEX_CAP, VetoEntry, account_scope,
    prune_expired,
};
use worker::kv::KvStore;

const INDEX_COOLDOWNS_KEY: &str = "index:cooldowns";
const INDEX_SEEN_KEY: &str = "index:seen";
const INDEX_PREPS_KEY: &str = "index:preps";
const INDEX_VETOS_KEY: &str = "index:vetos";

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

    fn veto_key(account: Option<&str>, instrument: &str, name: &str) -> String {
        let scope = account_scope(account);
        format!("veto:{scope}:{instrument}:{name}")
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
    serde_json::from_str::<Vec<T>>(&text)
        .map_err(|e| StateError::Backend(format!("decode {key}: {e}")))
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
        instrument: &str,
        name: &str,
        ttl_seconds: u64,
    ) -> Result<(), StateError> {
        let key = Self::veto_key(account, instrument, name);
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
            !(e.instrument == instrument && e.name == name && e.account == account_owned)
        });
        entries.push(VetoEntry {
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
        instrument: &str,
        name: &str,
    ) -> Result<bool, StateError> {
        // Global vetos affect every account; check the global key
        // first, then the account-scoped key if one was requested. Two
        // GETs in the rare-collision case; one GET on the common path
        // where there's no account-specific veto.
        let global = Self::veto_key(None, instrument, name);
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
            let scoped = Self::veto_key(account, instrument, name);
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
        instrument: &str,
        name: &str,
    ) -> Result<bool, StateError> {
        // Scoped clear — clearing on one account doesn't touch a
        // different account's veto or the global veto.
        let key = Self::veto_key(account, instrument, name);
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
            !(e.instrument == instrument && e.name == name && e.account == account_owned)
        });
        if entries.len() != before || was {
            self.write_veto_index(&entries).await?;
        }
        Ok(was)
    }

    async fn snapshot(&self) -> Result<Snapshot, StateError> {
        let now: DateTime<Utc> = Utc::now();
        let cooldowns = prune_expired(self.read_cooldown_index().await?, now);
        let recent_seen = prune_expired(self.read_seen_index().await?, now);
        let preps = prune_expired(self.read_prep_index().await?, now);
        let vetos = prune_expired(self.read_veto_index().await?, now);
        Ok(Snapshot {
            now,
            cooldowns,
            recent_seen,
            preps,
            vetos,
        })
    }

    // TODO(1b): real KV-backed impls for the max_retries surface land in
    // sub-step 1b. For now the stubs return safe defaults (no attempts,
    // never-seen) so the type-checker passes — they are not wired into
    // `run_enter` yet, so behaviour is unchanged.
    async fn record_entry_attempt(&self, _attempt: EntryAttempt) -> Result<(), StateError> {
        Err(StateError::Backend(
            "record_entry_attempt: not implemented (TODO 1b)".into(),
        ))
    }

    async fn list_entry_attempts(
        &self,
        _account: Option<&str>,
        _trade_id: &str,
    ) -> Result<Vec<EntryAttempt>, StateError> {
        Ok(Vec::new())
    }

    async fn set_entry_attempt_broker_trade_id(
        &self,
        _account: Option<&str>,
        _trade_id: &str,
        _attempt_no: u32,
        _broker_trade_id: &str,
    ) -> Result<(), StateError> {
        Err(StateError::Backend(
            "set_entry_attempt_broker_trade_id: not implemented (TODO 1b)".into(),
        ))
    }

    async fn is_retry_fire_seen(
        &self,
        _account: Option<&str>,
        _trade_id: &str,
        _shell_time: DateTime<Utc>,
    ) -> Result<bool, StateError> {
        Ok(false)
    }

    async fn mark_retry_fire_seen(
        &self,
        _account: Option<&str>,
        _trade_id: &str,
        _shell_time: DateTime<Utc>,
        _ttl_seconds: u64,
    ) -> Result<(), StateError> {
        Err(StateError::Backend(
            "mark_retry_fire_seen: not implemented (TODO 1b)".into(),
        ))
    }
}
