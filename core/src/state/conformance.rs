//! Cross-backend conformance harness for [`StateStore`].
//!
//! Every behavioural rule a store must honour — account scoping, per-trade_id
//! isolation, TTL/fail-open semantics, list ordering, idempotent clears — is
//! asserted **once** here, against an abstract `&impl StateStore`. Both backends
//! run the identical assertions:
//!
//! - `core`'s own tests drive these against [`MemStateStore`] (via
//!   `pollster::block_on`), so the in-memory reference can never silently drift.
//! - `trade-control-worker`'s tests drive the same functions against
//!   `PgStateStore` (via `#[tokio::test]`), so Postgres is held to byte-for-byte
//!   the same contract.
//!
//! This is the parity gate for the Cloudflare-KV → VM-Postgres migration: if a
//! function here passes on Mem but fails on Pg (or vice versa), the backends
//! disagree and the bug is caught before it reaches a live trade.
//!
//! # What is *not* covered here
//!
//! - **Snapshot.** [`StateStore::snapshot`] is a real query on Pg but returns
//!   empty sections on Mem by design (Mem is a flat KV with no secondary index).
//!   Pg-only; tested in the worker crate.
//! - **Backend internals.** A handful of Mem tests reach into `expiry_of` to
//!   assert a row's stored TTL is far-future (bug #15). That's a white-box check
//!   of one backend's storage, not a behavioural contract, so it stays
//!   backend-local. The black-box consequence (the row is still readable long
//!   after a control-TTL window) *is* asserted here.
//! - **Pure (de)serialisation round-trips** of the `*Entry` structs — those
//!   don't touch a store and live in `core`'s unit tests.
//!
//! # Conventions
//!
//! Each function is self-contained and uses **unique ids** (callers pass a
//! `tag`) so the suite is safe to run against a *shared, persistent* Postgres
//! dev database without cross-test collisions. `Utc::now()` is the clock; TTL
//! expiry is exercised with `ttl = 0` (expires the instant it's written) or a
//! past `now` stamp, both of which read as expired on any wall-clock backend.

use chrono::{SubsecRound, Utc};

use super::*;
use crate::control_event::ControlKind;
use crate::plan_state::Phase;

/// "Now", truncated to **microsecond** precision.
///
/// `MemStateStore`/KV store timestamps as RFC3339 strings (nanosecond-exact),
/// but Postgres `timestamptz` columns only hold microseconds — so a raw
/// `Utc::now()` (nanosecond) written to Pg and read back loses its sub-µs tail
/// and fails an exact `assert_eq!` against the Mem copy. No worker decision
/// compares timestamps below microsecond granularity (bar/signal times are
/// second-granular; the ns tail is a `Utc::now()` artifact, never a threshold),
/// so the *contract under test* is "same instant to storage precision". Feeding
/// every asserted-equal timestamp through this makes both backends store a
/// byte-identical value and the equality exact on each — without weakening the
/// assertion to an approximate compare. Pure TTL/expiry stamps (never read back
/// for equality) don't need it, but using it uniformly keeps the harness
/// simple.
fn now_us() -> chrono::DateTime<Utc> {
    Utc::now().trunc_subsecs(6)
}

/// Run every conformance check against `store`, namespacing all ids with `tag`
/// so concurrent backends (and repeat runs on a persistent db) never collide.
///
/// `tag` should be short and unique per backend/run, e.g. `"mem"` or
/// `"pg-1234"`. The caller owns the async runtime; this just awaits in order.
pub async fn run_all(store: &impl StateStore, tag: &str) {
    seen(store, tag).await;
    cooldown(store, tag).await;
    prep(store, tag).await;
    veto(store, tag).await;
    prep_block(store, tag).await;
    pause(store, tag).await;
    news_window(store, tag).await;
    entry_attempt(store, tag).await;
    retry_fire(store, tag).await;
    spread_blackout_window(store, tag).await;
    blackout_windows(store, tag).await;
    spread_blackout_record(store, tag).await;
    mw_state(store, tag).await;
    trade_plan(store, tag).await;
    plan_state(store, tag).await;
    control_events(store, tag).await;
    archived_plan(store, tag).await;
}

// ---------------------------------------------------------------------------
// seen
// ---------------------------------------------------------------------------

/// `seen` round-trips, forgets idempotently, and a ttl-0 mark reads as expired.
pub async fn seen(store: &impl StateStore, tag: &str) {
    let id = format!("{tag}-seen-1");
    let now = now_us();

    assert!(!store.is_seen(&id).await.unwrap(), "absent id is not seen");
    store
        .mark_seen(&id, Action::Prep, now, "ok", 3600, None)
        .await
        .unwrap();
    assert!(store.is_seen(&id).await.unwrap(), "marked id is seen");

    store.forget_seen(&id).await.unwrap();
    assert!(
        !store.is_seen(&id).await.unwrap(),
        "forgotten id is not seen"
    );
    // Idempotent: forgetting again is a no-op.
    store.forget_seen(&id).await.unwrap();

    // ttl 0 → expires_at == seen_at == now; reads back already expired.
    let exp_id = format!("{tag}-seen-exp");
    store
        .mark_seen(&exp_id, Action::Enter, now, "entered", 0, Some("T-1"))
        .await
        .unwrap();
    assert!(
        !store.is_seen(&exp_id).await.unwrap(),
        "ttl-0 id is already expired"
    );
}

// ---------------------------------------------------------------------------
// cooldown
// ---------------------------------------------------------------------------

/// Cooldown scoping: per-account isolation, and a global (`None`) cooldown
/// pauses every account.
pub async fn cooldown(store: &impl StateStore, tag: &str) {
    let instr = format!("{tag}-CD");
    let now = now_us();

    // Scoped per account.
    store
        .set_cooldown(Some("acct-a"), &instr, 1, now)
        .await
        .unwrap();
    assert!(store.is_cooled_down(Some("acct-a"), &instr).await.unwrap());
    assert!(
        !store.is_cooled_down(Some("acct-b"), &instr).await.unwrap(),
        "a cooldown on acct-a must not pause acct-b"
    );
    assert!(!store.is_cooled_down(None, &instr).await.unwrap());
    store.clear_cooldown(Some("acct-a"), &instr).await.unwrap();
    assert!(!store.is_cooled_down(Some("acct-a"), &instr).await.unwrap());

    // Global cooldown covers every account.
    let g = format!("{tag}-CDG");
    store.set_cooldown(None, &g, 1, now).await.unwrap();
    assert!(store.is_cooled_down(Some("acct-a"), &g).await.unwrap());
    assert!(store.is_cooled_down(Some("acct-b"), &g).await.unwrap());
    store.clear_cooldown(None, &g).await.unwrap();
}

// ---------------------------------------------------------------------------
// prep
// ---------------------------------------------------------------------------

/// Prep scoping (per-account isolation, global satisfies every account),
/// scoped clear returns the setter id and leaves other scopes alone, and a
/// re-set overwrites the timestamp.
pub async fn prep(store: &impl StateStore, tag: &str) {
    let instr = format!("{tag}-PREP");
    let now = now_us();

    // Scoped per account — the 2026-06 bug fix.
    store
        .set_prep(Some("acct-a"), &instr, "break-and-close", now, 3600, "id-a")
        .await
        .unwrap();
    assert_eq!(
        store
            .get_prep(Some("acct-a"), &instr, "break-and-close")
            .await
            .unwrap(),
        Some(now),
        "prep visible on the account that set it"
    );
    assert_eq!(
        store
            .get_prep(Some("acct-b"), &instr, "break-and-close")
            .await
            .unwrap(),
        None,
        "prep on acct-a must NOT satisfy acct-b"
    );
    assert_eq!(
        store
            .get_prep(None, &instr, "break-and-close")
            .await
            .unwrap(),
        None,
        "a scoped prep must not register as global"
    );

    // Scoped clear returns the setter id and is per-scope.
    store
        .set_prep(Some("acct-b"), &instr, "break-and-close", now, 3600, "id-b")
        .await
        .unwrap();
    let cleared = store
        .clear_prep(Some("acct-a"), &instr, "break-and-close")
        .await
        .unwrap();
    assert_eq!(cleared.as_deref(), Some("id-a"));
    assert!(
        store
            .get_prep(Some("acct-b"), &instr, "break-and-close")
            .await
            .unwrap()
            .is_some(),
        "acct-b's prep survives an acct-a clear"
    );
    store
        .clear_prep(Some("acct-b"), &instr, "break-and-close")
        .await
        .unwrap();

    // Global prep satisfies every account.
    let g = format!("{tag}-PREPG");
    store
        .set_prep(None, &g, "break", now, 3600, "id-g")
        .await
        .unwrap();
    assert_eq!(
        store.get_prep(Some("acct-a"), &g, "break").await.unwrap(),
        Some(now)
    );
    store.clear_prep(None, &g, "break").await.unwrap();

    // Re-set overwrites the timestamp.
    let later = now + chrono::Duration::minutes(5);
    store
        .set_prep(None, &g, "retest", now, 3600, "id-1")
        .await
        .unwrap();
    store
        .set_prep(None, &g, "retest", later, 3600, "id-2")
        .await
        .unwrap();
    assert_eq!(
        store.get_prep(None, &g, "retest").await.unwrap(),
        Some(later),
        "re-set must overwrite the stored timestamp"
    );
    store.clear_prep(None, &g, "retest").await.unwrap();

    // Absent prep reads None.
    assert_eq!(
        store
            .get_prep(None, &format!("{tag}-PREP-absent"), "x")
            .await
            .unwrap(),
        None
    );
}

// ---------------------------------------------------------------------------
// veto
// ---------------------------------------------------------------------------

/// Veto round-trip plus the full scoping matrix: per-trade_id isolation (the
/// 2026-06-11 fix), per-account isolation, global covers every account, and a
/// scoped clear leaves other scopes intact.
pub async fn veto(store: &impl StateStore, tag: &str) {
    let instr = format!("{tag}-VETO");
    let ttl = 6 * 3600;

    // Round-trip with a global veto.
    store
        .set_veto(None, "t1", &instr, "news-window", ttl)
        .await
        .unwrap();
    assert!(
        store
            .is_vetoed(None, "t1", &instr, "news-window")
            .await
            .unwrap()
    );
    assert!(
        store
            .clear_veto(None, "t1", &instr, "news-window")
            .await
            .unwrap()
    );
    assert!(
        !store
            .is_vetoed(None, "t1", &instr, "news-window")
            .await
            .unwrap()
    );

    // Scoped per trade_id — a veto under trade-a must not block trade-b.
    store
        .set_veto(Some("acct-a"), "trade-a", &instr, "too-high", ttl)
        .await
        .unwrap();
    assert!(
        store
            .is_vetoed(Some("acct-a"), "trade-a", &instr, "too-high")
            .await
            .unwrap()
    );
    assert!(
        !store
            .is_vetoed(Some("acct-a"), "trade-b", &instr, "too-high")
            .await
            .unwrap(),
        "veto under trade-a must NOT block trade-b"
    );
    store
        .clear_veto(Some("acct-a"), "trade-a", &instr, "too-high")
        .await
        .unwrap();

    // Scoped per account.
    store
        .set_veto(Some("acct-a"), "t1", &instr, "news", ttl)
        .await
        .unwrap();
    assert!(
        !store
            .is_vetoed(Some("acct-b"), "t1", &instr, "news")
            .await
            .unwrap(),
        "veto on acct-a must NOT bleed into acct-b"
    );
    assert!(
        !store.is_vetoed(None, "t1", &instr, "news").await.unwrap(),
        "a scoped veto must not register as global"
    );

    // Global covers every account.
    store
        .set_veto(None, "t1", &instr, "halt", ttl)
        .await
        .unwrap();
    assert!(
        store
            .is_vetoed(Some("acct-a"), "t1", &instr, "halt")
            .await
            .unwrap()
    );
    assert!(
        store
            .is_vetoed(Some("acct-b"), "t1", &instr, "halt")
            .await
            .unwrap()
    );

    // Scoped clear is per-scope: clearing acct-a + global leaves acct-b's own.
    store
        .set_veto(Some("acct-b"), "t1", &instr, "news", ttl)
        .await
        .unwrap();
    assert!(
        store
            .clear_veto(Some("acct-a"), "t1", &instr, "news")
            .await
            .unwrap()
    );
    assert!(store.clear_veto(None, "t1", &instr, "halt").await.unwrap());
    assert!(
        store
            .is_vetoed(Some("acct-b"), "t1", &instr, "news")
            .await
            .unwrap(),
        "acct-b's scoped veto survives an acct-a + global clear"
    );
    store
        .clear_veto(Some("acct-b"), "t1", &instr, "news")
        .await
        .unwrap();
    store
        .clear_veto(Some("acct-a"), "t1", &instr, "news")
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// prep_block
// ---------------------------------------------------------------------------

/// Prep-block round-trip, global-first lookup (a global block covers a scoped
/// query), and scoped isolation (a scoped block doesn't leak across accounts).
pub async fn prep_block(store: &impl StateStore, tag: &str) {
    let instr = format!("{tag}-PB");
    let now = now_us();

    // Round-trip (global).
    store
        .block_prep(None, &instr, "too-late", now, 3600)
        .await
        .unwrap();
    assert!(
        store
            .is_prep_blocked(None, &instr, "too-late")
            .await
            .unwrap()
    );
    store
        .clear_prep_block(None, &instr, "too-late")
        .await
        .unwrap();
    assert!(
        !store
            .is_prep_blocked(None, &instr, "too-late")
            .await
            .unwrap()
    );

    // Global block covers a scoped query (global-first).
    store
        .block_prep(None, &instr, "halt", now, 3600)
        .await
        .unwrap();
    assert!(
        store
            .is_prep_blocked(Some("acct-a"), &instr, "halt")
            .await
            .unwrap(),
        "a global prep-block must cover an account-scoped query"
    );
    store.clear_prep_block(None, &instr, "halt").await.unwrap();

    // Scoped block does not leak across accounts.
    store
        .block_prep(Some("acct-a"), &instr, "scoped", now, 3600)
        .await
        .unwrap();
    assert!(
        store
            .is_prep_blocked(Some("acct-a"), &instr, "scoped")
            .await
            .unwrap()
    );
    assert!(
        !store
            .is_prep_blocked(Some("acct-b"), &instr, "scoped")
            .await
            .unwrap(),
        "an acct-a prep-block must not block acct-b"
    );
    assert!(
        !store.is_prep_blocked(None, &instr, "scoped").await.unwrap(),
        "a scoped prep-block must not register as global"
    );
    store
        .clear_prep_block(Some("acct-a"), &instr, "scoped")
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// pause
// ---------------------------------------------------------------------------

/// Pause round-trip, multiple concurrent blackouts per trade (clearing one
/// leaves the other), and per-trade_id isolation.
pub async fn pause(store: &impl StateStore, tag: &str) {
    let trade = format!("{tag}-pause-1");
    let now = now_us();

    store
        .set_pause(&trade, "nfp", Some("news:USD-NFP"), now, 6 * 3600)
        .await
        .unwrap();
    let listed = store.list_pauses_for_trade(&trade).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].blackout_id, "nfp");
    assert_eq!(listed[0].reason.as_deref(), Some("news:USD-NFP"));

    // Second concurrent blackout on the same trade.
    store
        .set_pause(&trade, "cb-rate", Some("news:ECB"), now, 6 * 3600)
        .await
        .unwrap();
    assert_eq!(store.list_pauses_for_trade(&trade).await.unwrap().len(), 2);

    // Clear only nfp — cb-rate remains.
    assert!(store.clear_pause(&trade, "nfp").await.unwrap());
    let listed = store.list_pauses_for_trade(&trade).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].blackout_id, "cb-rate");
    // Clearing an absent pause is a no-op.
    assert!(!store.clear_pause(&trade, "nfp").await.unwrap());
    assert!(store.clear_pause(&trade, "cb-rate").await.unwrap());

    // Isolated per trade_id.
    let other = format!("{tag}-pause-2");
    store
        .set_pause(&trade, "b1", None, now, 6 * 3600)
        .await
        .unwrap();
    assert_eq!(store.list_pauses_for_trade(&trade).await.unwrap().len(), 1);
    assert!(
        store
            .list_pauses_for_trade(&other)
            .await
            .unwrap()
            .is_empty()
    );
    store.clear_pause(&trade, "b1").await.unwrap();
}

// ---------------------------------------------------------------------------
// news_window
// ---------------------------------------------------------------------------

/// News-window round-trip and that it lives in a namespace distinct from
/// pauses (setting a pause must not surface in the news listing).
pub async fn news_window(store: &impl StateStore, tag: &str) {
    let trade = format!("{tag}-news-1");
    let now = now_us();

    store
        .set_news_window(&trade, "usd-nfp", Some("USD-NFP"), now, 3600)
        .await
        .unwrap();
    let listed = store.list_news_windows_for_trade(&trade).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].news_id, "usd-nfp");
    assert_eq!(listed[0].reason.as_deref(), Some("USD-NFP"));
    assert!(store.clear_news_window(&trade, "usd-nfp").await.unwrap());
    assert!(
        store
            .list_news_windows_for_trade(&trade)
            .await
            .unwrap()
            .is_empty()
    );
    // Clearing an absent window is a no-op.
    assert!(!store.clear_news_window(&trade, "usd-nfp").await.unwrap());

    // News and pause namespaces are independent.
    store
        .set_pause(&trade, "nfp", None, now, 3600)
        .await
        .unwrap();
    assert!(
        store
            .list_news_windows_for_trade(&trade)
            .await
            .unwrap()
            .is_empty(),
        "a pause must not surface in the news listing"
    );
    store.clear_pause(&trade, "nfp").await.unwrap();
}

// ---------------------------------------------------------------------------
// entry_attempt
// ---------------------------------------------------------------------------

fn sample_attempt(
    trade_id: &str,
    account: Option<&str>,
    instrument: &str,
    attempt_no: u32,
    broker_order_id: &str,
) -> EntryAttempt {
    let now = now_us();
    EntryAttempt {
        trade_id: trade_id.into(),
        account: account.map(|s| s.to_string()),
        instrument: instrument.into(),
        attempt_no,
        broker_order_id: broker_order_id.into(),
        broker_trade_id: None,
        direction: Direction::Long,
        placed_at: now,
        shell_time: now,
        expires_at: now + chrono::Duration::hours(24),
        stop_loss_price: None,
        cancel_at: None,
        pip_size: None,
        blackout_close: BlackoutCloseAction::default(),
        breakeven: None,
    }
}

/// Entry-attempt record/list (ordered by attempt_no), per-account isolation,
/// and setting the broker_trade_id (idempotent, no-op on a missing attempt_no).
pub async fn entry_attempt(store: &impl StateStore, tag: &str) {
    let instr = format!("{tag}-EA");
    let tid = format!("{tag}-ea-order");

    // Round-trip + ordering: insert out of order, list comes back sorted.
    store
        .record_entry_attempt(sample_attempt(&tid, None, &instr, 3, "ord-3"))
        .await
        .unwrap();
    store
        .record_entry_attempt(sample_attempt(&tid, None, &instr, 1, "ord-1"))
        .await
        .unwrap();
    store
        .record_entry_attempt(sample_attempt(&tid, None, &instr, 2, "ord-2"))
        .await
        .unwrap();
    let got = store.list_entry_attempts(None, &tid).await.unwrap();
    assert_eq!(
        got.iter().map(|a| a.attempt_no).collect::<Vec<_>>(),
        vec![1, 2, 3],
        "attempts list ascending by attempt_no"
    );

    // set broker_trade_id: idempotent, no-op on a missing attempt_no.
    store
        .set_entry_attempt_broker_trade_id(None, &tid, 1, "trade-xyz")
        .await
        .unwrap();
    store
        .set_entry_attempt_broker_trade_id(None, &tid, 99, "trade-zzz")
        .await
        .unwrap();
    let got = store.list_entry_attempts(None, &tid).await.unwrap();
    assert_eq!(
        got.iter()
            .find(|a| a.attempt_no == 1)
            .unwrap()
            .broker_trade_id
            .as_deref(),
        Some("trade-xyz")
    );

    // Isolated per account; the global scope sees neither.
    let shared = format!("{tag}-ea-shared");
    store
        .record_entry_attempt(sample_attempt(&shared, Some("acct-a"), &instr, 1, "ord-a1"))
        .await
        .unwrap();
    store
        .record_entry_attempt(sample_attempt(&shared, Some("acct-b"), &instr, 1, "ord-b1"))
        .await
        .unwrap();
    let a = store
        .list_entry_attempts(Some("acct-a"), &shared)
        .await
        .unwrap();
    let b = store
        .list_entry_attempts(Some("acct-b"), &shared)
        .await
        .unwrap();
    assert_eq!(a.len(), 1);
    assert_eq!(b.len(), 1);
    assert_eq!(a[0].broker_order_id, "ord-a1");
    assert_eq!(b[0].broker_order_id, "ord-b1");
    assert!(
        store
            .list_entry_attempts(None, &shared)
            .await
            .unwrap()
            .is_empty(),
        "scoped attempts must not bleed into the global scope"
    );

    // list_all includes every scope's attempts for this run's trade ids.
    let all = store.list_all_entry_attempts().await.unwrap();
    assert!(
        all.iter().filter(|a| a.trade_id == shared).count() == 2,
        "list_all_entry_attempts recovers both accounts' rows"
    );

    // Cleanup so a persistent db doesn't accumulate.
    store.delete_entry_attempt(None, &tid, 1).await.unwrap();
    store.delete_entry_attempt(None, &tid, 2).await.unwrap();
    store.delete_entry_attempt(None, &tid, 3).await.unwrap();
    store
        .delete_entry_attempt(Some("acct-a"), &shared, 1)
        .await
        .unwrap();
    store
        .delete_entry_attempt(Some("acct-b"), &shared, 1)
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// retry_fire
// ---------------------------------------------------------------------------

/// Retry-fire-seen round-trip (a distinct shell_time is independent) and
/// per-account isolation.
pub async fn retry_fire(store: &impl StateStore, tag: &str) {
    let tid = format!("{tag}-rf");
    let shell_time = now_us();
    let other = shell_time + chrono::Duration::hours(1);

    assert!(
        !store
            .is_retry_fire_seen(None, &tid, shell_time)
            .await
            .unwrap()
    );
    store
        .mark_retry_fire_seen(None, &tid, shell_time, 3600)
        .await
        .unwrap();
    assert!(
        store
            .is_retry_fire_seen(None, &tid, shell_time)
            .await
            .unwrap()
    );
    assert!(
        !store.is_retry_fire_seen(None, &tid, other).await.unwrap(),
        "a different shell_time is independent"
    );

    // Isolated per account.
    let tid2 = format!("{tag}-rf2");
    store
        .mark_retry_fire_seen(Some("acct-a"), &tid2, shell_time, 3600)
        .await
        .unwrap();
    assert!(
        store
            .is_retry_fire_seen(Some("acct-a"), &tid2, shell_time)
            .await
            .unwrap()
    );
    assert!(
        !store
            .is_retry_fire_seen(Some("acct-b"), &tid2, shell_time)
            .await
            .unwrap()
    );
    assert!(
        !store
            .is_retry_fire_seen(None, &tid2, shell_time)
            .await
            .unwrap()
    );
}

// ---------------------------------------------------------------------------
// spread_blackout_window (singleton)
// ---------------------------------------------------------------------------

/// The singleton spread-blackout window: absent reads `None`, a write opens it,
/// and a window stamped far enough in the past reads back closed (fail-open).
/// New coverage — the Mem suite never exercised this family.
///
/// NOTE: this family clamps a tiny ttl up to `MIN_TTL_SECONDS` (60s) on *both*
/// backends, so a `ttl = 0` write does NOT expire immediately — it stays open
/// for 60s. Expiry is therefore exercised with a past `opened_at`, not ttl-0.
/// (That clamp parity is itself a thing the harness pins: Pg gained the floor
/// to match Mem.)
pub async fn spread_blackout_window(store: &impl StateStore, _tag: &str) {
    let now = now_us();

    store
        .set_spread_blackout_window(now, 6 * 3600)
        .await
        .unwrap();
    let got = store
        .get_spread_blackout_window()
        .await
        .unwrap()
        .expect("window open after set");
    assert_eq!(got.opened_at, now);

    // Stamp an hour in the past with the minimum ttl → expired → reads closed.
    let past = now - chrono::Duration::hours(1);
    store.set_spread_blackout_window(past, 1).await.unwrap();
    assert!(
        store.get_spread_blackout_window().await.unwrap().is_none(),
        "a window whose clamped ttl has elapsed reads back closed (fail-open)"
    );
}

// ---------------------------------------------------------------------------
// blackout_windows (per-instrument market hours)
// ---------------------------------------------------------------------------

/// Market-hours blackout windows: per-instrument keys don't collide, overwrite
/// replaces, an unwritten instrument is fail-open (empty), and an expired write
/// reads back empty.
pub async fn blackout_windows(store: &impl StateStore, tag: &str) {
    let us500 = format!("{tag}-US500");
    let gold = format!("{tag}-GOLD");
    let now = now_us();

    // Absent → fail-open.
    assert!(store.get_blackout_windows(&us500).await.unwrap().is_empty());

    let w_us = [NoEntryWindow::new(18 * 60, 2 * 60)];
    let w_gold = [
        NoEntryWindow::new(21 * 60, 23 * 60),
        NoEntryWindow::new(3 * 60, 4 * 60),
    ];
    store
        .set_blackout_windows(&us500, &w_us, now, 26 * 3600)
        .await
        .unwrap();
    store
        .set_blackout_windows(&gold, &w_gold, now, 26 * 3600)
        .await
        .unwrap();

    // Per-instrument keys don't collide.
    assert_eq!(
        store.get_blackout_windows(&us500).await.unwrap(),
        w_us.to_vec()
    );
    assert_eq!(
        store.get_blackout_windows(&gold).await.unwrap(),
        w_gold.to_vec()
    );

    // Overwrite replaces.
    let revised = [NoEntryWindow::new(19 * 60, 60)];
    store
        .set_blackout_windows(&us500, &revised, now, 26 * 3600)
        .await
        .unwrap();
    assert_eq!(
        store.get_blackout_windows(&us500).await.unwrap(),
        revised.to_vec()
    );

    // Expired write → fail-open (empty).
    let stale = format!("{tag}-STALE");
    let past = now - chrono::Duration::days(2);
    store
        .set_blackout_windows(&stale, &w_us, past, 26 * 3600)
        .await
        .unwrap();
    assert!(
        store.get_blackout_windows(&stale).await.unwrap().is_empty(),
        "an expired window reads as no-blackout (fail-open)"
    );
}

// ---------------------------------------------------------------------------
// spread_blackout_record (per-trade)
// ---------------------------------------------------------------------------

fn sample_record(trade_id: &str, account: Option<&str>, instrument: &str) -> SpreadBlackoutRecord {
    let now = now_us();
    SpreadBlackoutRecord {
        trade_id: trade_id.into(),
        instrument: instrument.into(),
        account: account.map(|s| s.to_string()),
        applied: true,
        opened_at: now,
        expires_at: now + chrono::Duration::hours(6),
        pip_size: 0.0001,
        original_stops: Vec::new(),
        cancelled_orders: Vec::new(),
    }
}

/// Per-trade spread-blackout record: upsert/get round-trip, upsert overwrites,
/// `list_all` recovers every active record, and clear removes one. New
/// coverage — the Mem suite never exercised this family.
pub async fn spread_blackout_record(store: &impl StateStore, tag: &str) {
    let instr = format!("{tag}-SBR");
    let tid = format!("{tag}-sbr-1");

    assert!(
        store
            .get_spread_blackout_record(&tid)
            .await
            .unwrap()
            .is_none(),
        "absent record reads None"
    );

    let rec = sample_record(&tid, Some("reversals"), &instr);
    store
        .upsert_spread_blackout_record(&rec, 6 * 3600)
        .await
        .unwrap();
    let got = store
        .get_spread_blackout_record(&tid)
        .await
        .unwrap()
        .expect("record present after upsert");
    assert_eq!(got.instrument, instr);
    assert_eq!(got.account.as_deref(), Some("reversals"));
    assert!(got.applied);

    // Upsert overwrites (flip applied false, change pip).
    let mut revised = rec.clone();
    revised.applied = false;
    revised.pip_size = 0.01;
    store
        .upsert_spread_blackout_record(&revised, 6 * 3600)
        .await
        .unwrap();
    let got = store
        .get_spread_blackout_record(&tid)
        .await
        .unwrap()
        .unwrap();
    assert!(!got.applied);
    assert_eq!(got.pip_size, 0.01);

    // list_all recovers it.
    let all = store.list_all_spread_blackout_records().await.unwrap();
    assert!(
        all.iter().any(|r| r.trade_id == tid),
        "list_all_spread_blackout_records includes the active record"
    );

    // Clear removes; clearing again is a no-op.
    store.clear_spread_blackout_record(&tid).await.unwrap();
    assert!(
        store
            .get_spread_blackout_record(&tid)
            .await
            .unwrap()
            .is_none()
    );
    store.clear_spread_blackout_record(&tid).await.unwrap();
}

// ---------------------------------------------------------------------------
// mw_state
// ---------------------------------------------------------------------------

/// M/W geometry round-trip, global-first lookup (a scoped query finds a global
/// row), upsert overwrites, and clear is idempotent.
pub async fn mw_state(store: &impl StateStore, tag: &str) {
    let tid = format!("{tag}-mw-1");
    let now = now_us();

    assert_eq!(store.get_mw_state(None, &tid).await.unwrap(), None);

    let state = MwState {
        neckline: 1.1120,
        right_shoulder: Some(1.1185),
        updated_at: now,
        expires_at: now + chrono::Duration::hours(6),
    };
    store
        .upsert_mw_state(None, &tid, &state, 6 * 3600)
        .await
        .unwrap();
    let got = store.get_mw_state(None, &tid).await.unwrap().unwrap();
    assert_eq!(got.neckline, 1.1120);
    assert_eq!(got.right_shoulder, Some(1.1185));

    // Global-first: a scoped query finds the global row.
    let got_scoped = store
        .get_mw_state(Some("reversals"), &tid)
        .await
        .unwrap()
        .expect("scoped query falls back to global row");
    assert_eq!(got_scoped.neckline, 1.1120);

    // Upsert overwrites.
    let revised = MwState {
        neckline: 1.1105,
        right_shoulder: None,
        updated_at: now,
        expires_at: now + chrono::Duration::hours(6),
    };
    store
        .upsert_mw_state(None, &tid, &revised, 6 * 3600)
        .await
        .unwrap();
    let got = store.get_mw_state(None, &tid).await.unwrap().unwrap();
    assert_eq!(got.neckline, 1.1105);
    assert_eq!(got.right_shoulder, None);

    // Clear is idempotent.
    store.clear_mw_state(None, &tid).await.unwrap();
    assert_eq!(store.get_mw_state(None, &tid).await.unwrap(), None);
    store.clear_mw_state(None, &tid).await.unwrap();
}

// ---------------------------------------------------------------------------
// trade_plan
// ---------------------------------------------------------------------------

fn sample_plan(trade_id: &str) -> TradePlan {
    let json = format!(
        r#"{{"trade_id":"{trade_id}","instrument":"EUR_USD","direction":"short",
            "granularity":"h1","pip_size":0.0001,"rules":[]}}"#
    );
    serde_json::from_str(&json).unwrap()
}

/// Trade-plan round-trip with account scoping: global + scoped coexist, a
/// scoped get is NOT global-first (the engine always knows the carrier scope),
/// `list_all` recovers both scopes, a registered plan reads back long after a
/// control-TTL window (bug #15 — no short TTL leaked), and clear removes the
/// live key but a separately archived copy survives.
pub async fn trade_plan(store: &impl StateStore, tag: &str) {
    let global = format!("{tag}-plan-global");
    let scoped = format!("{tag}-plan-scoped");

    store
        .put_trade_plan(None, &sample_plan(&global))
        .await
        .unwrap();
    store
        .put_trade_plan(Some("reversals"), &sample_plan(&scoped))
        .await
        .unwrap();

    let got = store
        .get_trade_plan(Some("reversals"), &scoped)
        .await
        .unwrap()
        .expect("scoped plan present");
    assert_eq!(got.trade_id, scoped);
    // Scoped get is NOT global-first.
    assert!(
        store
            .get_trade_plan(Some("reversals"), &global)
            .await
            .unwrap()
            .is_none(),
        "a global plan must not satisfy a scoped get"
    );

    // list_all recovers both scopes with the right account tags.
    let all = store.list_all_trade_plans().await.unwrap();
    let g = all
        .iter()
        .find(|s| s.plan.trade_id == global)
        .expect("global plan listed");
    assert_eq!(g.account, None);
    let s = all
        .iter()
        .find(|s| s.plan.trade_id == scoped)
        .expect("scoped plan listed");
    assert_eq!(s.account.as_deref(), Some("reversals"));

    // Bug #15: a registered plan reads back well past any control-TTL window —
    // the black-box consequence of the no-short-TTL stamp. (The white-box
    // far-future-expiry assertion stays backend-local.)
    let got_global = store
        .get_trade_plan(None, &global)
        .await
        .unwrap()
        .expect("registered plan still present (no control-TTL leak)");
    assert_eq!(got_global.trade_id, global);

    // Clear removes the live key but keeps a separately archived copy.
    let archive_me = format!("{tag}-plan-archive");
    let plan = sample_plan(&archive_me);
    let final_state = PlanState::seed(Phase::Done, now_us());
    store.put_trade_plan(None, &plan).await.unwrap();
    store
        .archive_plan(None, &plan, &final_state, now_us())
        .await
        .unwrap();
    store.clear_trade_plan(None, &archive_me).await.unwrap();
    assert!(
        store
            .get_trade_plan(None, &archive_me)
            .await
            .unwrap()
            .is_none(),
        "clear removes the live plan key"
    );
    assert!(
        store
            .list_all_archived_plans()
            .await
            .unwrap()
            .iter()
            .any(|a| a.plan.trade_id == archive_me),
        "the archived copy survives the live clear"
    );

    // Cleanup live keys.
    store.clear_trade_plan(None, &global).await.unwrap();
    store
        .clear_trade_plan(Some("reversals"), &scoped)
        .await
        .unwrap();
    store.clear_archived_plan(None, &archive_me).await.unwrap();
}

// ---------------------------------------------------------------------------
// plan_state
// ---------------------------------------------------------------------------

/// Plan-state round-trip (full struct fidelity through the store) and
/// idempotent clear.
pub async fn plan_state(store: &impl StateStore, tag: &str) {
    let tid = format!("{tag}-ps-1");
    let mut st = PlanState::seed(Phase::AwaitBreakAndClose, now_us());
    st.watermark = Some(now_us() - chrono::Duration::hours(2));
    st.last_close.insert("01-veto-too-high".into(), 1.2345);

    store
        .put_plan_state(Some("reversals"), &tid, &st)
        .await
        .unwrap();
    let got = store
        .get_plan_state(Some("reversals"), &tid)
        .await
        .unwrap()
        .expect("plan-state present");
    assert_eq!(got, st);

    store
        .clear_plan_state(Some("reversals"), &tid)
        .await
        .unwrap();
    assert!(
        store
            .get_plan_state(Some("reversals"), &tid)
            .await
            .unwrap()
            .is_none()
    );
    store
        .clear_plan_state(Some("reversals"), &tid)
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// control_events
// ---------------------------------------------------------------------------

/// Control-event append/list (ascending by set_at), per-trade_id scoping, and
/// clear drops all of a trade's events.
pub async fn control_events(store: &impl StateStore, tag: &str) {
    let tid = format!("{tag}-ce-1");

    let a = ControlEvent::new(
        ControlKind::Veto,
        "too-low",
        "GBP/USD",
        now_us(),
        3600,
        None,
    );
    let b = ControlEvent::new(
        ControlKind::Cooldown,
        "",
        "GBP/USD",
        now_us() + chrono::Duration::hours(1),
        8 * 3600,
        Some("req-1".into()),
    );
    // Insert out of order; list returns ascending by set_at.
    store
        .record_control_event(Some("reversals"), &tid, &b)
        .await
        .unwrap();
    store
        .record_control_event(Some("reversals"), &tid, &a)
        .await
        .unwrap();
    let got = store
        .list_control_events(Some("reversals"), &tid)
        .await
        .unwrap();
    assert_eq!(got, vec![a.clone(), b.clone()]);

    // Scoping: a different trade_id sees none of these.
    let other = format!("{tag}-ce-other");
    assert!(
        store
            .list_control_events(Some("reversals"), &other)
            .await
            .unwrap()
            .is_empty()
    );

    // Clear drops all of this trade's events.
    store
        .clear_control_events(Some("reversals"), &tid)
        .await
        .unwrap();
    assert!(
        store
            .list_control_events(Some("reversals"), &tid)
            .await
            .unwrap()
            .is_empty()
    );
}

// ---------------------------------------------------------------------------
// archived_plan
// ---------------------------------------------------------------------------

/// Archived-plan round-trip across scopes: `list_all` recovers global +
/// scoped, the account is recovered from the key (not the skipped body), the
/// terminal state survives, and a scoped delete leaves the other scope intact.
pub async fn archived_plan(store: &impl StateStore, tag: &str) {
    let global = format!("{tag}-arch-global");
    let scoped = format!("{tag}-arch-scoped");

    let mut term = PlanState::seed(Phase::Done, now_us());
    term.fired.insert("01-veto-too-high".into());
    let when = now_us();

    store
        .archive_plan(None, &sample_plan(&global), &term, when)
        .await
        .unwrap();
    store
        .archive_plan(Some("reversals"), &sample_plan(&scoped), &term, when)
        .await
        .unwrap();

    let all = store.list_all_archived_plans().await.unwrap();
    let g = all
        .iter()
        .find(|a| a.plan.trade_id == global)
        .expect("global archived plan listed");
    assert_eq!(g.account, None);
    let s = all
        .iter()
        .find(|a| a.plan.trade_id == scoped)
        .expect("scoped archived plan listed");
    assert_eq!(s.account.as_deref(), Some("reversals"));
    assert_eq!(s.final_state.phase, Phase::Done);
    assert!(s.final_state.fired.contains("01-veto-too-high"));

    // Scoped delete leaves the other scope intact.
    store
        .clear_archived_plan(Some("reversals"), &scoped)
        .await
        .unwrap();
    let remaining = store.list_all_archived_plans().await.unwrap();
    assert!(remaining.iter().any(|a| a.plan.trade_id == global));
    assert!(!remaining.iter().any(|a| a.plan.trade_id == scoped));

    // Cleanup.
    store.clear_archived_plan(None, &global).await.unwrap();
}
