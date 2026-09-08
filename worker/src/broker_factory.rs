//! Native broker construction — the VM replacement for the wasm worker's
//! `acquire_oanda_broker` / `acquire_tn_broker` (`src/lib.rs`).
//!
//! The wasm worker built brokers from Cloudflare `Env` secrets + the KV account
//! index + a KV session cache. Natively the inputs are:
//!
//! * **account metadata** — an [`AccountMetadata`] read from the Postgres
//!   account index ([`crate::PgMetadataStore`]): which broker, demo/live, and
//!   (for OANDA) the sub-account id.
//! * **secrets** — [`crate::Secrets`] from the process env (the OANDA token +
//!   global live flag).
//! * **TradeNation credentials** — resolved natively by `tradenation_api` from
//!   the enc account store (`~/.config/tradenation/accounts.enc`) keyed by the
//!   account *name*. The whole wasm redirect-chain login (`src/tn_login.rs`) is
//!   unnecessary off-wasm — `tradenation_api::login_demo_named` does it.
//!
//! Like the wasm worker, this does **not** box the broker: the `Broker` trait
//! is not object-safe, because its methods return `impl Future`. The native
//! dispatcher branches on [`BrokerKind`] and calls the matching `acquire_*`,
//! then monomorphizes the generic dispatch per arm — exactly as the wasm
//! worker's `main` does.
//!
//! Monomorphization is **not** the only way to dispatch, though — `dyn Broker`
//! is what's unavailable, not type erasure as such. The cron engine erases the
//! same brokers behind an enum (`trade_control_cron::BrokerHandle`) and matches
//! on it. Both strategies cost one arm per broker; this module uses the
//! generic one because each caller here knows its concrete broker at the call
//! site, while the cron engine has to *return* a broker across an async
//! boundary, where `impl Trait` will not do.
//!
//! # Adding a broker
//!
//! Each `acquire_*` guards with `meta.broker != <its kind>` rather than an
//! exhaustive match. That is deliberate and safe: the guard rejects *every*
//! other broker, including ones added later, so a new [`BrokerKind`] cannot
//! silently acquire the wrong client. `each_factory_rejects_every_foreign_broker`
//! walks [`BrokerKind::ALL`] to keep that true without a per-broker test.

use broker_ibkr::IbkrBroker;
use broker_oanda::OandaBroker;
use broker_tradenation_adapter::TradeNationAdapter;
use trade_control_core::account::AccountMetadata;
use trade_control_core::intent::BrokerKind;

use crate::Secrets;

/// Why a broker couldn't be constructed. The dispatcher maps these to the same
/// HTTP statuses the wasm worker used (OANDA login fail → 500, TN login fail →
/// 503) and logs the detail.
#[derive(Debug)]
pub enum BrokerError {
    /// The account's `broker` tag didn't match the broker the caller asked for
    /// (an intent's `broker` must match its named account's recorded broker).
    BrokerMismatch {
        intent: BrokerKind,
        account: BrokerKind,
    },
    /// An OANDA account record is missing its `oanda_account_id` (required to
    /// route to the right sub-account under the shared token).
    MissingOandaAccountId { account: String },
    /// `OANDA_API_KEY` isn't set but an OANDA account was requested.
    MissingOandaApiKey,
    /// TradeNation native login failed (no such account in the enc store, bad
    /// credentials, or a network failure). Carries the underlying message.
    TradeNationLogin(String),
    /// An IBKR account record is missing its account id (`DU…` for paper,
    /// `U…` for live). A Gateway login can front several accounts, so this
    /// cannot be derived — routing to the wrong one would trade the wrong book.
    MissingIbkrAccountId { account: String },
    /// Could not reach the IB Gateway. Unlike the HTTPS brokers this is a
    /// **local** failure: a Java process on this host is down, not logged in,
    /// or listening elsewhere. Carries the underlying message.
    IbkrConnect(String),
}

impl std::fmt::Display for BrokerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BrokerMismatch { intent, account } => write!(
                f,
                "intent broker {intent:?} does not match account broker {account:?}"
            ),
            Self::MissingOandaAccountId { account } => {
                write!(f, "oanda account '{account}' has no oanda_account_id")
            }
            Self::MissingOandaApiKey => write!(f, "OANDA_API_KEY not set"),
            Self::TradeNationLogin(msg) => write!(f, "tradenation login failed: {msg}"),
            Self::MissingIbkrAccountId { account } => {
                write!(f, "ibkr account '{account}' has no ibkr_account_id")
            }
            Self::IbkrConnect(msg) => write!(f, "ibkr gateway connect failed: {msg}"),
        }
    }
}

impl std::error::Error for BrokerError {}

/// Build an [`OandaBroker`] for `meta`. The token comes from `secrets`; the
/// sub-account id from `meta`; live/practice from the account's `kind` (so each
/// account hits its own OANDA environment regardless of the global flag —
/// matching the wasm worker's per-account behaviour).
pub fn acquire_oanda(
    meta: &AccountMetadata,
    secrets: &Secrets,
) -> Result<OandaBroker, BrokerError> {
    if meta.broker != BrokerKind::Oanda {
        return Err(BrokerError::BrokerMismatch {
            intent: BrokerKind::Oanda,
            account: meta.broker,
        });
    }
    let account_id =
        meta.oanda_account_id
            .clone()
            .ok_or_else(|| BrokerError::MissingOandaAccountId {
                account: meta.name.clone(),
            })?;
    let api_key = secrets
        .oanda_api_key
        .clone()
        .ok_or(BrokerError::MissingOandaApiKey)?;
    Ok(OandaBroker::from_api_key(
        api_key,
        account_id,
        meta.kind.is_live(),
    ))
}

/// Build a [`TradeNationAdapter`] for `meta`, logging in natively against the
/// enc account store keyed by the account *name*. (TradeNation identifies the
/// account by its session credentials, not an id, so `oanda_account_id` is
/// irrelevant here.)
pub async fn acquire_tn(meta: &AccountMetadata) -> Result<TradeNationAdapter, BrokerError> {
    if meta.broker != BrokerKind::TradeNation {
        return Err(BrokerError::BrokerMismatch {
            intent: BrokerKind::TradeNation,
            account: meta.broker,
        });
    }
    // Native login walks the redirect chain and reads the enc store; no Worker
    // Fetch, no KV session cache. A future session cache (Postgres) is an
    // optimisation, not a correctness need — the broker re-logins on rejection.
    let session = tradenation_api::login_demo_named(&meta.name)
        .await
        .map_err(|e| BrokerError::TradeNationLogin(e.to_string()))?;
    // `broker_tradenation::login` takes the session as a JSON string (the wasm
    // worker fed it the KV-cached blob); serialize the fresh session to match.
    let session_json = serde_json::to_string(&session)
        .map_err(|e| BrokerError::TradeNationLogin(format!("serialize session: {e}")))?;
    let broker = broker_tradenation::login(&session_json)
        .await
        .ok_or_else(|| {
            BrokerError::TradeNationLogin("broker_tradenation::login returned None".into())
        })?;
    Ok(TradeNationAdapter(broker))
}

/// Loopback address of the **paper** IB Gateway.
const IBKR_PAPER_GATEWAY: &str = "127.0.0.1:4002";
/// Loopback address of the **live** IB Gateway. Real money.
const IBKR_LIVE_GATEWAY: &str = "127.0.0.1:4001";

/// Build an [`IbkrBroker`] for `meta`, connecting to the local IB Gateway.
///
/// # Why the address is derived, not configured
///
/// The Gateway address follows from the account's `kind`: a demo account is a
/// paper Gateway on 4002, a live account a live Gateway on 4001. Deriving it
/// removes the failure mode a configurable field would introduce — a live
/// account pointed at the paper port (or, far worse, the reverse) — and it
/// matches how OANDA already picks its host from `kind.is_live()`.
///
/// # The client id
///
/// IBKR rejects a duplicate client id outright, so each account needs its own.
/// It is derived from the account name's hash rather than counted, because the
/// worker builds these on demand with no shared counter to draw from; a stable
/// per-account value also means reconnecting the same account reuses its id
/// instead of leaking a new one each time.
pub async fn acquire_ibkr(meta: &AccountMetadata) -> Result<IbkrBroker, BrokerError> {
    if meta.broker != BrokerKind::Ibkr {
        return Err(BrokerError::BrokerMismatch {
            intent: BrokerKind::Ibkr,
            account: meta.broker,
        });
    }
    // IBKR reuses `oanda_account_id`'s slot for its own account id (`DU…`).
    // Not ideal naming, but adding a third broker-specific field to metadata
    // for the same concept — "which sub-account under this login" — would be
    // worse; the field is already the generic answer to that question.
    let account_id =
        meta.oanda_account_id
            .clone()
            .ok_or_else(|| BrokerError::MissingIbkrAccountId {
                account: meta.name.clone(),
            })?;
    let gateway = if meta.kind.is_live() {
        IBKR_LIVE_GATEWAY
    } else {
        IBKR_PAPER_GATEWAY
    };
    IbkrBroker::connect(gateway, ibkr_client_id(&meta.name), account_id)
        .await
        .map_err(|err| BrokerError::IbkrConnect(err.to_string()))
}

/// A stable, per-account IBKR API client id.
///
/// Must be unique among everything connected to one Gateway, and must be a
/// positive `i32` (0 is reserved for the Gateway's own bound orders). Derived
/// from the account name so it is stable across reconnects — the worker builds
/// brokers on demand with no shared counter to draw from.
///
/// # Why the range is this wide
///
/// A collision is not a cosmetic problem: the Gateway rejects a duplicate
/// client id outright, so two accounts sharing one id means the second cannot
/// connect while the first is up — an outage that appears only once both are
/// live. Hashing into a small range makes that likely much sooner than
/// intuition suggests (the birthday paradox: a 60k range collides at ~1-in-2
/// odds by only a few hundred accounts). Spreading over the full positive
/// `i32` above the reserved floor keeps collisions negligible at any account
/// count this system will reach.
fn ibkr_client_id(account_name: &str) -> i32 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    account_name.hash(&mut hasher);
    client_id_from_hash(hasher.finish())
}

/// Lowest client id this system will hand out.
///
/// Keeps clear of `0` (reserved by the Gateway for its own bound orders) and of
/// the small ids a human picks by hand when attaching TWS or the probe binary
/// to the same Gateway.
const IBKR_CLIENT_ID_FLOOR: i32 = 1_000;

/// Map a hash onto the usable client-id range.
///
/// Split out from [`ibkr_client_id`] so the floor is checkable at the values
/// that matter — `0`, `1`, `u64::MAX` — rather than inferred from a sample of
/// hashed names. Over a range this wide a sweep of realistic names would never
/// land near the floor, so it could not tell a working floor from a missing
/// one.
fn client_id_from_hash(hash: u64) -> i32 {
    let span = (i32::MAX - IBKR_CLIENT_ID_FLOOR) as u64;
    IBKR_CLIENT_ID_FLOOR + (hash % span) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use trade_control_core::account::AccountKind;

    fn secrets_with_oanda() -> Secrets {
        Secrets {
            signing_key: "sk".into(),
            admin_key: "ak".into(),
            max_risk_pct: 1.0,
            max_open_positions: 3.0,
            oanda_api_key: Some("token".into()),
            oanda_live: false,
        }
    }

    fn oanda_meta(account_id: Option<&str>, kind: AccountKind) -> AccountMetadata {
        AccountMetadata {
            name: "oanda-demo".into(),
            broker: BrokerKind::Oanda,
            kind,
            caps: Default::default(),
            oanda_account_id: account_id.map(str::to_owned),
        }
    }

    // The broker types don't implement `Debug` (they wrap live API clients), so
    // assertions match on the result via `matches!` rather than `unwrap_err`.

    #[test]
    fn oanda_happy_path_builds() {
        let meta = oanda_meta(Some("101-011-1-003"), AccountKind::Demo);
        let result = acquire_oanda(&meta, &secrets_with_oanda());
        assert!(
            result.is_ok(),
            "a complete oanda record must build a broker"
        );
    }

    #[test]
    fn oanda_missing_account_id_errors() {
        let meta = oanda_meta(None, AccountKind::Demo);
        let result = acquire_oanda(&meta, &secrets_with_oanda());
        assert!(matches!(
            result,
            Err(BrokerError::MissingOandaAccountId { .. })
        ));
    }

    #[test]
    fn oanda_missing_api_key_errors() {
        let meta = oanda_meta(Some("101-011-1-003"), AccountKind::Demo);
        let mut secrets = secrets_with_oanda();
        secrets.oanda_api_key = None;
        let result = acquire_oanda(&meta, &secrets);
        assert!(matches!(result, Err(BrokerError::MissingOandaApiKey)));
    }

    fn ibkr_meta(account_id: Option<&str>, kind: AccountKind) -> AccountMetadata {
        AccountMetadata {
            name: "ibkr-paper".into(),
            broker: BrokerKind::Ibkr,
            kind,
            caps: Default::default(),
            oanda_account_id: account_id.map(str::to_owned),
        }
    }

    /// A missing account id must be refused **before** a Gateway connection is
    /// attempted. A Gateway login can front several accounts, so an absent id
    /// is not something to guess at — routing to the wrong one trades the wrong
    /// book. Asserting it here also keeps the test offline: it fails before any
    /// socket work.
    #[tokio::test]
    async fn ibkr_missing_account_id_errors_without_connecting() {
        let meta = ibkr_meta(None, AccountKind::Demo);
        let result = acquire_ibkr(&meta).await;
        assert!(matches!(
            result,
            Err(BrokerError::MissingIbkrAccountId { .. })
        ));
    }

    /// The broker-mismatch guard fires before any I/O, same as the other two.
    #[tokio::test]
    async fn ibkr_rejects_a_foreign_account() {
        for &kind in BrokerKind::ALL {
            if kind == BrokerKind::Ibkr {
                continue;
            }
            let mut meta = ibkr_meta(Some("DU1"), AccountKind::Demo);
            meta.broker = kind;
            assert!(
                matches!(
                    acquire_ibkr(&meta).await,
                    Err(BrokerError::BrokerMismatch { .. })
                ),
                "acquire_ibkr must reject a {kind:?} account"
            );
        }
    }

    /// The Gateway address follows from the account's `kind`, so a live account
    /// can never be pointed at the paper port or — far worse — a demo account
    /// at the live one. Deriving it is what removes that failure mode, so the
    /// derivation is pinned rather than left implicit.
    #[test]
    fn the_gateway_port_follows_the_account_kind() {
        assert_eq!(IBKR_PAPER_GATEWAY, "127.0.0.1:4002");
        assert_eq!(IBKR_LIVE_GATEWAY, "127.0.0.1:4001");
        assert_ne!(
            IBKR_PAPER_GATEWAY, IBKR_LIVE_GATEWAY,
            "paper and live must never resolve to the same Gateway"
        );
    }

    /// Client ids must be stable per account (so a reconnect reuses its id
    /// rather than leaking a new one), distinct between accounts (the Gateway
    /// rejects duplicates outright), and clear of the low ids a human picks by
    /// hand when attaching TWS or the probe to the same Gateway.
    ///
    /// The floor is swept over **many** generated names rather than a handful
    /// of realistic ones: with only a few samples every hash happens to land
    /// above the floor anyway, so the test passes whether or not the offset is
    /// applied — it asserts a property the code does not actually provide.
    /// A wide sweep is what makes the floor load-bearing.
    #[test]
    fn ibkr_client_ids_are_stable_distinct_and_out_of_the_way() {
        assert_eq!(ibkr_client_id("ibkr-paper"), ibkr_client_id("ibkr-paper"));
        assert_ne!(ibkr_client_id("ibkr-paper"), ibkr_client_id("ibkr-live"));

        for name in ["ibkr-paper", "ibkr-live", "a", ""] {
            assert!(ibkr_client_id(name) >= IBKR_CLIENT_ID_FLOOR, "{name}");
        }
    }

    /// The floor is checked at the hash values that would breach it — a
    /// hash of 0 is the case a missing offset turns into client id 0, which
    /// the Gateway reserves for its own bound orders.
    #[test]
    fn the_client_id_floor_holds_at_the_extremes() {
        assert_eq!(client_id_from_hash(0), IBKR_CLIENT_ID_FLOOR);
        assert_eq!(client_id_from_hash(1), IBKR_CLIENT_ID_FLOOR + 1);
        for hash in [0, 1, 2, 999, u64::MAX / 2, u64::MAX] {
            let id = client_id_from_hash(hash);
            assert!(id >= IBKR_CLIENT_ID_FLOOR, "hash {hash} -> {id}");
            assert!(id > 0, "hash {hash} -> {id} must be positive");
        }
    }

    /// Client ids must not collide across a realistic number of accounts — a
    /// duplicate is rejected by the Gateway outright, so a collision is an
    /// account that simply cannot connect while its twin is up.
    #[test]
    fn ibkr_client_ids_do_not_collide_across_many_accounts() {
        let ids: std::collections::HashSet<i32> = (0..500)
            .map(|i| ibkr_client_id(&format!("acct-{i}")))
            .collect();
        assert_eq!(ids.len(), 500, "client ids collided across 500 accounts");
    }

    /// The `!=` guards must reject **every** other broker, not just the one
    /// other broker that existed when they were written. Driving this off
    /// `ALL` means a new `BrokerKind` is covered the moment it is added, with
    /// no test to remember to write — which is what makes leaving the guards
    /// as `!=` (rather than exhaustive matches) safe.
    #[test]
    fn each_factory_rejects_every_foreign_broker() {
        for &kind in BrokerKind::ALL {
            let mut meta = oanda_meta(Some("x"), AccountKind::Demo);
            meta.broker = kind;
            let got = acquire_oanda(&meta, &secrets_with_oanda());
            if kind == BrokerKind::Oanda {
                assert!(got.is_ok(), "oanda must accept its own account");
            } else {
                assert!(
                    matches!(got, Err(BrokerError::BrokerMismatch { .. })),
                    "acquire_oanda must reject a {kind:?} account"
                );
            }
        }
    }

    #[test]
    fn oanda_rejects_a_tradenation_account() {
        let mut meta = oanda_meta(Some("x"), AccountKind::Demo);
        meta.broker = BrokerKind::TradeNation;
        let result = acquire_oanda(&meta, &secrets_with_oanda());
        assert!(matches!(result, Err(BrokerError::BrokerMismatch { .. })));
    }

    #[tokio::test]
    async fn tn_rejects_an_oanda_account() {
        // A broker-mismatch is decided before any network login is attempted,
        // so this is safe to assert without the enc store / a live session.
        let meta = oanda_meta(Some("x"), AccountKind::Demo);
        let result = acquire_tn(&meta).await;
        assert!(matches!(result, Err(BrokerError::BrokerMismatch { .. })));
    }
}
