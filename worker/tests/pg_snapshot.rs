//! Pg-only test for [`StateStore::snapshot`].
//!
//! `snapshot()` aggregates *active* control rows across families with real
//! `SELECT … WHERE expires_at > now()` queries — the secondary-query capability
//! Postgres has natively and the KV store faked with `index:*` JSON blobs. It is
//! deliberately NOT in the cross-backend conformance harness because
//! `MemStateStore::snapshot` returns empty sections by design (a flat KV with no
//! secondary index), so there's nothing to hold the two backends to here.
//!
//! The snapshot is worker-wide and unscoped (every account's active rows), so
//! this test tags its writes uniquely and filters the returned sections to its
//! own tag — safe against a shared, persistent dev DB with other rows present.

use chrono::{SubsecRound, Utc};
use trade_control_core::intent::Action;
use trade_control_core::state::StateStore;
use trade_control_worker::PgStateStore;

fn test_db_url() -> String {
    std::env::var("TEST_DATABASE_URL").unwrap_or_else(|_| {
        "postgresql://candle_cache:candle_cache@localhost:5432/trade_control_dev".to_string()
    })
}

async fn store() -> PgStateStore {
    let store = PgStateStore::connect(&test_db_url())
        .await
        .expect("connect to test db");
    store.migrate().await.expect("run migrations");
    store
}

#[tokio::test]
async fn snapshot_surfaces_active_rows_and_omits_expired() {
    let store = store().await;
    let tag = format!("snap-{}", Utc::now().timestamp_nanos_opt().unwrap_or(0));
    let now = Utc::now().trunc_subsecs(6);

    // One active row in each of several families…
    let cd = format!("{tag}-CD");
    let prep_i = format!("{tag}-PREP");
    let pb_i = format!("{tag}-PB");
    let veto_tid = format!("{tag}-vtid");
    let veto_i = format!("{tag}-VI");
    let pause_tid = format!("{tag}-ptid");
    let news_tid = format!("{tag}-ntid");
    let seen_id = format!("{tag}-seen");

    store
        .set_cooldown(Some("acct-a"), &cd, 1, now)
        .await
        .unwrap();
    store
        .set_prep(None, &prep_i, "break", now, 3600, "id-1")
        .await
        .unwrap();
    store
        .block_prep(None, &pb_i, "too-late", now, 3600)
        .await
        .unwrap();
    store
        .set_veto(Some("acct-a"), &veto_tid, &veto_i, "news", 3600)
        .await
        .unwrap();
    store
        .set_pause(&pause_tid, "nfp", Some("news:USD"), now, 3600)
        .await
        .unwrap();
    store
        .set_news_window(&news_tid, "usd-nfp", Some("USD"), now, 3600)
        .await
        .unwrap();
    store
        .mark_seen(&seen_id, Action::Prep, now, "ok", 3600, None)
        .await
        .unwrap();

    // …and one already-expired row that must NOT surface (ttl-0 on `seen`,
    // which doesn't clamp, so it's expired the instant it's read back).
    let expired_seen = format!("{tag}-seen-expired");
    store
        .mark_seen(&expired_seen, Action::Prep, now, "ok", 0, None)
        .await
        .unwrap();

    let snap = store.snapshot().await.unwrap();

    // Active rows present (filter to this run's tag — the snapshot is global).
    assert!(
        snap.cooldowns.iter().any(|c| c.instrument == cd),
        "active cooldown surfaces in snapshot"
    );
    assert!(
        snap.preps.iter().any(|p| p.instrument == prep_i),
        "active prep surfaces"
    );
    assert!(
        snap.prep_blocks.iter().any(|p| p.instrument == pb_i),
        "active prep-block surfaces"
    );
    assert!(
        snap.vetos.iter().any(|v| v.trade_id == veto_tid),
        "active veto surfaces"
    );
    assert!(
        snap.pauses.iter().any(|p| p.trade_id == pause_tid),
        "active pause surfaces"
    );
    assert!(
        snap.news_windows.iter().any(|n| n.trade_id == news_tid),
        "active news window surfaces"
    );
    assert!(
        snap.recent_seen.iter().any(|s| s.id == seen_id),
        "active seen row surfaces"
    );

    // Expired row absent.
    assert!(
        !snap.recent_seen.iter().any(|s| s.id == expired_seen),
        "an expired seen row must NOT surface in the snapshot"
    );

    // Cleanup this run's rows so the shared dev db doesn't accumulate.
    store.clear_cooldown(Some("acct-a"), &cd).await.unwrap();
    store.clear_prep(None, &prep_i, "break").await.unwrap();
    store
        .clear_prep_block(None, &pb_i, "too-late")
        .await
        .unwrap();
    store
        .clear_veto(Some("acct-a"), &veto_tid, &veto_i, "news")
        .await
        .unwrap();
    store.clear_pause(&pause_tid, "nfp").await.unwrap();
    store.clear_news_window(&news_tid, "usd-nfp").await.unwrap();
    store.forget_seen(&seen_id).await.unwrap();
    store.forget_seen(&expired_seen).await.unwrap();
}
