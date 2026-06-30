//! Cross-backend conformance harness for [`MetadataStore`].
//!
//! One set of assertions, run against both `MemMetadataStore` (the reference)
//! and the native `PgMetadataStore`, so the two implementations can't drift.
//! Same parity-as-test design as [`crate::state::conformance`].
//!
//! Every account name is `tag`-namespaced so the harness is safe to run against
//! a shared, persistent dev database with other rows present and against the
//! in-memory store in the same test process. Each family cleans up after itself
//! (`remove`) so a re-run starts clean.

use super::caps::AccountCaps;
use super::kind::AccountKind;
use super::metadata::{AccountMetadata, MetadataError, MetadataStore};
use crate::intent::BrokerKind;

/// Run every metadata-store assertion against `store`. `tag` namespaces all
/// account names so concurrent / shared-DB runs don't collide. Panics on the
/// first failed assertion (it's a test harness).
pub async fn run_all(store: &impl MetadataStore, tag: &str) {
    add_get_round_trip(store, tag).await;
    add_duplicate_errors(store, tag).await;
    get_missing_errors(store, tag).await;
    remove_missing_errors(store, tag).await;
    remove_drops_record(store, tag).await;
    list_is_name_sorted(store, tag).await;
    oanda_fields_round_trip(store, tag).await;
    caps_round_trip(store, tag).await;
}

/// A tradenation demo record namespaced by `tag`.
fn tn_demo(tag: &str, name: &str) -> AccountMetadata {
    AccountMetadata::new(
        format!("{tag}-{name}"),
        BrokerKind::TradeNation,
        AccountKind::Demo,
    )
}

async fn add_get_round_trip(store: &impl MetadataStore, tag: &str) {
    let m = tn_demo(tag, "rt");
    store.add(m.clone()).await.expect("add");
    let got = store.get(&m.name).await.expect("get back");
    assert_eq!(got, m, "[{tag}] add→get must round-trip exactly");
    store.remove(&m.name).await.expect("cleanup");
}

async fn add_duplicate_errors(store: &impl MetadataStore, tag: &str) {
    let m = tn_demo(tag, "dup");
    store.add(m.clone()).await.expect("first add");
    let err = store
        .add(m.clone())
        .await
        .expect_err("[{tag}] adding the same name twice must error");
    assert!(
        matches!(err, MetadataError::AlreadyExists(name) if name == m.name),
        "[{tag}] duplicate add must be AlreadyExists with the name"
    );
    store.remove(&m.name).await.expect("cleanup");
}

async fn get_missing_errors(store: &impl MetadataStore, tag: &str) {
    let name = format!("{tag}-ghost");
    let err = store
        .get(&name)
        .await
        .expect_err("[{tag}] get of an unknown name must error");
    assert!(
        matches!(err, MetadataError::NotFound(n) if n == name),
        "[{tag}] missing get must be NotFound with the name"
    );
}

async fn remove_missing_errors(store: &impl MetadataStore, tag: &str) {
    let name = format!("{tag}-ghost-rm");
    let err = store
        .remove(&name)
        .await
        .expect_err("[{tag}] remove of an unknown name must error");
    assert!(
        matches!(err, MetadataError::NotFound(n) if n == name),
        "[{tag}] missing remove must be NotFound with the name"
    );
}

async fn remove_drops_record(store: &impl MetadataStore, tag: &str) {
    let m = tn_demo(tag, "rmdrop");
    store.add(m.clone()).await.expect("add");
    store.remove(&m.name).await.expect("remove");
    let err = store
        .get(&m.name)
        .await
        .expect_err("[{tag}] removed record must be gone");
    assert!(
        matches!(err, MetadataError::NotFound(_)),
        "[{tag}] get after remove must be NotFound"
    );
}

async fn list_is_name_sorted(store: &impl MetadataStore, tag: &str) {
    // Insert out of order; `list` must come back name-ascending. The CLI's
    // `account list` is read by humans, so stable order is contractual.
    let names = ["lc", "la", "lb"];
    for n in names {
        store.add(tn_demo(tag, n)).await.expect("add");
    }
    let listed = store.list().await.expect("list");
    let mine: Vec<String> = listed
        .into_iter()
        .map(|m| m.name)
        .filter(|n| n.starts_with(&format!("{tag}-l")))
        .collect();
    let expected = vec![
        format!("{tag}-la"),
        format!("{tag}-lb"),
        format!("{tag}-lc"),
    ];
    assert_eq!(mine, expected, "[{tag}] list must be name-ascending");
    for n in names {
        store.remove(&format!("{tag}-{n}")).await.expect("cleanup");
    }
}

async fn oanda_fields_round_trip(store: &impl MetadataStore, tag: &str) {
    // An OANDA live account with a sub-account id — broker/kind/oanda_account_id
    // must all survive the column encode/decode.
    let m = AccountMetadata {
        name: format!("{tag}-oanda"),
        broker: BrokerKind::Oanda,
        kind: AccountKind::Live,
        caps: AccountCaps::default(),
        oanda_account_id: Some("101-011-31142393-003".to_string()),
    };
    store.add(m.clone()).await.expect("add oanda");
    let got = store.get(&m.name).await.expect("get oanda");
    assert_eq!(
        got, m,
        "[{tag}] oanda broker/kind/account-id must round-trip"
    );
    store.remove(&m.name).await.expect("cleanup");
}

async fn caps_round_trip(store: &impl MetadataStore, tag: &str) {
    // Non-default caps must survive the jsonb column.
    let m = AccountMetadata {
        name: format!("{tag}-caps"),
        broker: BrokerKind::TradeNation,
        kind: AccountKind::Live,
        caps: AccountCaps {
            max_risk_pct: Some(0.25),
            max_open_positions: Some(1),
        },
        oanda_account_id: None,
    };
    store.add(m.clone()).await.expect("add caps");
    let got = store.get(&m.name).await.expect("get caps");
    assert_eq!(got.caps, m.caps, "[{tag}] non-default caps must round-trip");
    assert_eq!(got, m, "[{tag}] full caps record must round-trip");
    store.remove(&m.name).await.expect("cleanup");
}
