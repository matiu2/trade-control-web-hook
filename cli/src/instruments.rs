//! Instrument-name validation and autocomplete for trade-control subcommands.
//!
//! Wraps [`tradenation_instrument_cache::InstrumentCache`] with sync helpers
//! the clap CLI can call directly. The cache crate is async (tokio); we spin
//! a current-thread runtime per call.
//!
//! OANDA is a no-op today — the user hasn't asked for OANDA-side validation
//! and OANDA's `EUR_USD`-style names already round-trip cleanly through
//! `tv_arm_hs.py`'s `instrument_for()`. The broker dispatch stays in place
//! so a future OANDA implementation slots in without touching callers.

use std::path::PathBuf;
use std::time::Duration;

use color_eyre::eyre::{Result, eyre};
use trade_control_core::intent::BrokerKind;
use tradenation_api::{login_demo, login_demo_named};
use tradenation_instrument_cache::{InstrumentCache, ResolveError};

/// How old the cached catalog can be before we re-walk the TN tree.
/// Per-instrument additions are rare; a week is conservative and still
/// catches new listings ahead of any real workflow.
const CACHE_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

/// Load the on-disk catalog (or fetch a fresh one when missing/stale).
/// `refresh = true` forces a re-walk even if the disk copy is fresh.
///
/// `account = Some(name)` logs in via the named entry in the local TN
/// store (so `tv-arm --account-id=X` walks the catalog under the same
/// identity that arms the trade). Errors when the name is missing
/// locally — the operator should `tradenation account create <name>`
/// first.
///
/// `account = None` keeps the legacy `login_demo()` behavior (picks the
/// default demo). Used by call sites that don't know an account yet,
/// e.g. the `status` instrument-footer.
///
/// `path` lets tests pin the cache to a tempdir. Pass `None` in production.
pub fn load_cache(
    refresh: bool,
    account: Option<&str>,
    path: Option<PathBuf>,
) -> Result<InstrumentCache> {
    // Validate the local-store entry up-front so the error message
    // doesn't get tangled with the redirect-chain noise from a
    // doomed login attempt.
    if let Some(name) = account {
        require_local_tn_account(name)?;
    }
    let account = account.map(str::to_owned);
    run_blocking(async move {
        let session = match account.as_deref() {
            Some(name) => login_demo_named(name).await.map_err(|e| {
                eyre!("logging in to TradeNation as '{name}' for catalog walk: {e}")
            })?,
            None => login_demo()
                .await
                .map_err(|e| eyre!("logging in to TradeNation for catalog walk: {e}"))?,
        };
        let mut cache = InstrumentCache::load_or_fetch(&session, CACHE_MAX_AGE, path)
            .await
            .map_err(|e| eyre!("loading TN instrument cache: {e}"))?;
        if refresh {
            cache
                .refresh(&session)
                .await
                .map_err(|e| eyre!("refreshing TN instrument cache: {e}"))?;
        }
        Ok(cache)
    })
}

/// Check that `name` exists in the local TN store; otherwise return an
/// actionable error pointing at `tradenation account create`. Kept
/// public so `account add` can run the same check before touching
/// Cloudflare.
pub fn require_local_tn_account(name: &str) -> Result<()> {
    let names = tradenation_api::accounts::list_accounts()
        .map_err(|e| eyre!("reading local TradeNation account store: {e}"))?;
    if names.iter().any(|(n, _)| n == name) {
        return Ok(());
    }
    Err(eyre!(
        "no TradeNation account named '{name}' in local store \
         (~/.config/tradenation/accounts.enc). \
         Create it first with `tradenation account create {name}` \
         (or pass --account-id matching an existing local entry)."
    ))
}

/// Validate a user-supplied instrument name for the given broker.
///
/// `account` is the operator-facing account name; for TradeNation it
/// drives the local-store login that walks the catalog (see
/// [`load_cache`]). `None` falls back to the default demo for callers
/// that don't have account context yet.
///
/// Return value:
/// - `Ok(None)` — exact match (or broker is OANDA, no validation done).
/// - `Ok(Some(canonical))` — the cache redirected (e.g. `"XAG/USD"` →
///   `"Spot Silver"`). Caller should swap their local binding to
///   `canonical` and log a `tracing::warn!` so the operator sees what
///   happened.
/// - `Err(report)` — no match. The error message already includes the
///   top candidates from `ResolveError::NotFound`.
pub fn validate_instrument(
    broker: BrokerKind,
    account: Option<&str>,
    name: &str,
) -> Result<Option<String>> {
    match broker {
        BrokerKind::Oanda => Ok(None),
        BrokerKind::TradeNation => {
            let cache = load_cache(false, account, None)?;
            resolve_with_cache(&cache, name)
        }
    }
}

/// Resolve `name` against an already-loaded cache. Split out from
/// [`validate_instrument`] so tests can drive it without a session.
pub fn resolve_with_cache(cache: &InstrumentCache, name: &str) -> Result<Option<String>> {
    match cache.resolve(name) {
        Ok(market) => {
            if market.name.eq_ignore_ascii_case(name.trim()) {
                Ok(None)
            } else {
                Ok(Some(market.name.clone()))
            }
        }
        Err(err @ ResolveError::NotFound { .. }) => Err(eyre!("{err}")),
    }
}

/// Run an async future on a fresh current-thread tokio runtime. We don't
/// hold the CLI long enough to warrant a worker-thread runtime.
pub(crate) fn run_blocking<F, T>(fut: F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| eyre!("building tokio runtime: {e}"))?;
    rt.block_on(fut)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tradenation_api::{Market, Session};
    use tradenation_instrument_cache::Catalog;

    fn stub(market_id: u64, name: &str, symbol: Option<&str>) -> Market {
        Market {
            market_id,
            quote_id: 1,
            name: name.to_string(),
            currency: "USD".to_string(),
            super_group_id: 1,
            spread: 0.0,
            margin: 0.0,
            bet_per: 0.0,
            decimal_places: 2,
            tradable: true,
            trade_on_web: true,
            bid: 0.0,
            ask: 0.0,
            symbol: symbol.map(str::to_string),
        }
    }

    /// Build an InstrumentCache pointing at a tempdir-seeded catalog.json.
    /// The seed bypasses the network walk so tests stay hermetic.
    fn seeded_cache(markets: Vec<Market>) -> (InstrumentCache, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("catalog.json");
        let cat = Catalog::new(markets);
        std::fs::write(&path, serde_json::to_vec(&cat).unwrap()).unwrap();
        let cache = run_blocking(async {
            // Generous max_age so the seeded catalog is treated as fresh.
            // Session is unused when the cache is fresh.
            let session = Session::demo("x", "x", "x", None);
            InstrumentCache::load_or_fetch(&session, Duration::from_secs(3600), Some(path.clone()))
                .await
                .map_err(|e| eyre!("seeded cache: {e}"))
        })
        .unwrap();
        (cache, tmp)
    }

    #[test]
    fn oanda_validation_is_noop() {
        // Any string passes; no network or cache touched.
        assert!(
            validate_instrument(BrokerKind::Oanda, None, "anything")
                .unwrap()
                .is_none()
        );
        assert!(
            validate_instrument(BrokerKind::Oanda, Some("reversals"), "")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn require_local_tn_account_errors_for_unknown() {
        // Pick a name with random suffix to keep the test stable
        // even if the user happens to have other named demos locally.
        let bogus = "nonexistent-tv-arm-test-account-zzz12345";
        let err = require_local_tn_account(bogus).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains(bogus), "got: {msg}");
        assert!(
            msg.contains("tradenation account create"),
            "expected fix-it hint; got: {msg}",
        );
    }

    #[test]
    fn exact_match_returns_none() {
        let (cache, _tmp) = seeded_cache(vec![stub(1, "Spot Gold", None)]);
        assert!(resolve_with_cache(&cache, "Spot Gold").unwrap().is_none());
        // Case-insensitive.
        assert!(resolve_with_cache(&cache, "spot gold").unwrap().is_none());
    }

    #[test]
    fn redirect_returns_canonical_name() {
        // XAG/USD with symbol XAGUSD redirects to "Spot Silver".
        let (cache, _tmp) = seeded_cache(vec![stub(2, "Spot Silver", Some("XAGUSD"))]);
        assert_eq!(
            resolve_with_cache(&cache, "XAG/USD").unwrap(),
            Some("Spot Silver".to_string())
        );
        // Plain XAGUSD resolves via symbol too.
        assert_eq!(
            resolve_with_cache(&cache, "XAGUSD").unwrap(),
            Some("Spot Silver".to_string())
        );
    }

    #[test]
    fn unknown_returns_error_with_candidates() {
        let (cache, _tmp) = seeded_cache(vec![
            stub(1, "Spot Gold", None),
            stub(2, "Spot Silver", Some("XAGUSD")),
        ]);
        let err = resolve_with_cache(&cache, "Spot Bronze").unwrap_err();
        let msg = format!("{err}");
        // The cache's ResolveError display already includes the candidate list.
        assert!(msg.contains("Spot Bronze"), "got: {msg}");
        assert!(
            msg.contains("Spot Gold") || msg.contains("Spot Silver"),
            "expected candidates in error message; got: {msg}",
        );
    }
}
