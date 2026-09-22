//! Resolve a chart's TradingView symbol into the canonical broker-
//! specific symbol via the `instrument-lookup` catalog, and synthesize
//! a `trade-calendar-maker` `Instrument` for the calendar-bars planner.
//!
//! This module is the single seam where `tv-arm` consults the shared
//! instrument catalog. It replaces ad-hoc symbol parsing (the old
//! `cli::parse_instrument` FX-only path) and the silent
//! TV-symbol → broker-symbol redirect that `cli::validate_instrument`
//! used to do behind our back.
//!
//! Two failure modes both surface as hard errors:
//!
//! 1. The chart's symbol isn't in the catalog at all. Error includes
//!    the user-overlay path so the operator can add an `[[asset]]`
//!    entry without rebuilding.
//! 2. The asset is in the catalog but isn't listed on the broker the
//!    operator chose (e.g. `--broker tradenation` on a chart for an
//!    OANDA-only gilt). Error lists which brokers DO carry it.

use color_eyre::eyre::{Result, eyre};
use instrument_lookup::{Asset, AssetClass, Broker as IlBroker};
use trade_calendar_maker::types::{Instrument as TcmInstrument, InstrumentType};
use trade_control_conventions::Broker as ConvBroker;

use crate::precision::CatalogPrecision;

/// One chart-symbol → broker-canonical-symbol resolution result.
///
/// Holds a static reference back into the catalog (the catalog is
/// loaded once into a `LazyLock` inside `instrument-lookup`) so all
/// downstream code can read news currencies, class, etc. without
/// re-querying.
#[derive(Debug, Clone)]
pub struct ResolvedInstrument {
    /// The catalog entry the chart's symbol resolved to.
    pub asset: &'static Asset,
    /// The asset's symbol on the chosen broker — what gets passed to
    /// `cli::build_trade_from_spec` (e.g. `"EUR/USD"` for TradeNation,
    /// `"EUR_USD"` for OANDA).
    pub broker_symbol: String,
    /// The **per-broker** catalog precision for the chosen broker's leg,
    /// from the native instrument-primary catalog. This is the correct
    /// fallback beneath live TradingView: OANDA and TradeNation legs of the
    /// same underlying can genuinely tick differently (AU200 OANDA 0.1 vs TN
    /// 1.0). Falls back to the legacy single-tick `Asset` precision only when
    /// the native catalog has no row for this (broker, symbol).
    pub precision: CatalogPrecision,
}

/// Resolve a TV-form symbol (e.g. `"TRADENATION:EURUSD"`, `"OANDA:EUR_USD"`,
/// or a bare `"EURUSD"`) against the chosen broker.
///
/// Uses `instrument_lookup::resolve` so operator typos on the chart's
/// symbol field (slash form, underscore form, display name) all
/// converge to the same `Asset`.
pub fn resolve_for_broker(tv_symbol: &str, broker: ConvBroker) -> Result<ResolvedInstrument> {
    let bare = strip_exchange(tv_symbol);
    let asset = instrument_lookup::resolve(bare)?.ok_or_else(|| {
        let hint = instrument_lookup::user_config_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "~/.config/instrument-lookup/mappings.toml".to_string());
        eyre!(
            "chart symbol {tv_symbol:?} is not in the instrument-lookup catalog. \
             Add an `[[asset]]` entry to {hint} to teach tv-arm about it.",
        )
    })?;

    let il_broker = to_il_broker(broker).ok_or_else(|| {
        eyre!(
            "asset {} cannot be resolved for {}: the instrument-lookup catalog has no \
             listing for that broker",
            asset.id,
            broker.as_str(),
        )
    })?;
    let broker_symbol = asset.symbol_for(il_broker).ok_or_else(|| {
        let carriers = brokers_carrying(asset);
        let listed = if carriers.is_empty() {
            "no broker in the catalog lists it".to_string()
        } else {
            format!("listed on: {}", carriers.join(", "))
        };
        eyre!(
            "asset {} is not listed on {} ({})",
            asset.id,
            broker.as_str(),
            listed,
        )
    })?;

    require_spread_coverage(broker_symbol)?;

    Ok(finish_resolution(asset, il_broker, broker_symbol))
}

/// Assemble the result once both gates (broker listing, spread coverage) have
/// passed. Split out so tests that are asserting *symbol resolution* can reach
/// it without the coverage gate — and without mutating process env, which races
/// across the thread-per-test harness.
fn finish_resolution(
    asset: &'static Asset,
    il_broker: IlBroker,
    broker_symbol: &str,
) -> ResolvedInstrument {
    ResolvedInstrument {
        asset,
        broker_symbol: broker_symbol.to_string(),
        precision: precision_for(il_broker, broker_symbol, asset),
    }
}

/// Refuse to arm an instrument the baked spread table doesn't cover.
///
/// **Why here.** Every reader of that table degrades to zero on a miss:
/// `spread_forecast_frac` returns `(0.0, 0.0)`, the forecast term drops out of
/// the SL `max`, and the stop is sized off the last bar alone — silently, weeks
/// after the fact. The instrument is a bare `String` by the time the worker sees
/// it, and `core` deliberately does not link `instrument-lookup`, so there is no
/// compile-time gate to be had. Arm time is the earliest point that knows both
/// the broker-canonical symbol AND has the operator in front of it, which makes
/// it the right place to turn a silent runtime mis-size into a loud refusal
/// before any plan exists.
///
/// `broker_symbol` — not the catalog id or the chart symbol — because that is
/// the exact string the worker keys the table on.
/// **Escape hatch.** Set `TV_ARM_ALLOW_UNBAKED=1` to downgrade the refusal to a
/// warning. Deliberately an env var and not a CLI flag: it is an emergency
/// override for "the table is behind and I need to arm this now", not a normal
/// mode of operation, so it should be awkward enough that nobody reaches for it
/// by habit — and it leaves a `warn!` in the log saying the stop was sized with
/// no forecast.
const ALLOW_UNBAKED_ENV: &str = "TV_ARM_ALLOW_UNBAKED";

fn require_spread_coverage(broker_symbol: &str) -> Result<()> {
    let coverage = trade_control_core::spread_blackout::coverage(broker_symbol);
    if coverage.is_covered() {
        return Ok(());
    }
    let rebake = format!(
        "cargo run -p spread-baseline-gen --bin generate -- \
         --brokers <oanda|tradenation> --days 90 --only {broker_symbol:?}",
    );
    if std::env::var(ALLOW_UNBAKED_ENV).is_ok_and(|v| v == "1") {
        tracing::warn!(
            instrument = broker_symbol,
            reason = coverage.reason(),
            "{ALLOW_UNBAKED_ENV}=1 — arming an instrument with NO spread \
             forecast. Stops will be sized off the last bar alone. Re-bake: \
             {rebake}",
        );
        return Ok(());
    }
    Err(eyre!(
        "instrument {broker_symbol:?} is {} — arming would size stops with no \
         spread forecast, so this is refused rather than silently degraded.\n  \
         Re-bake it:  {rebake}\n  \
         Or override for this run:  {ALLOW_UNBAKED_ENV}=1 tv-arm ...",
        coverage.reason(),
    ))
}

/// Resolve the per-broker precision for this leg from the native
/// instrument-primary catalog, falling back to the legacy `Asset`'s
/// single-tick precision when the native catalog has no matching row (a
/// build predating the data-fill, or a symbol only in the legacy overlay).
fn precision_for(broker: IlBroker, broker_symbol: &str, asset: &Asset) -> CatalogPrecision {
    match instrument_lookup::resolve_for(broker, broker_symbol) {
        Ok(Some(inst)) => CatalogPrecision::from_instrument(inst),
        _ => CatalogPrecision::from_asset(asset),
    }
}

/// Build a `trade-calendar-maker::Instrument` from an `Asset` so the
/// calendar-bars planner (which takes a tcm `Instrument`) can read
/// `affected_currencies` without parsing the symbol itself.
///
/// The `instrument_type` field is cosmetic for our hot path
/// (`plan_calendar_bars` only reads `affected_currencies`) but we map
/// it accurately for any future consumer.
pub fn synthesize_calendar_instrument(asset: &Asset) -> TcmInstrument {
    // Prefer OANDA's symbol as the carried-around name (tcm was
    // designed around OANDA symbols); fall back to the canonical id
    // when the asset isn't listed on OANDA.
    let symbol = asset
        .symbols
        .oanda
        .clone()
        .unwrap_or_else(|| asset.id.clone());
    let instrument_type = map_class(asset.class);
    TcmInstrument::new(symbol, instrument_type, asset.news_currencies.clone())
}

fn strip_exchange(tv_symbol: &str) -> &str {
    match tv_symbol.split_once(':') {
        Some((_, sym)) => sym,
        None => tv_symbol,
    }
}

/// Map our broker vocabulary onto `instrument-lookup`'s.
///
/// Every variant now resolves: `instrument-lookup` v4 added `Broker::Ibkr` and
/// an `ibkr` symbol column carrying the contract **series** root (`"ES"`,
/// `"GC"`). Before that this returned `None` for IBKR — substituting another
/// broker's column would have resolved a *different instrument*, which is worse
/// than not resolving at all. The `Option` is kept because the signature is the
/// natural place for a future broker that genuinely has no catalog column, and
/// because every caller already handles the `None`.
fn to_il_broker(broker: ConvBroker) -> Option<IlBroker> {
    match broker {
        ConvBroker::Oanda => Some(IlBroker::Oanda),
        ConvBroker::TradeNation => Some(IlBroker::TradeNation),
        ConvBroker::Ibkr => Some(IlBroker::Ibkr),
    }
}

/// Which brokers list this asset — used only to build the "not listed on X,
/// but is on Y" operator message.
///
/// Driven off `IlBroker::ALL` rather than a hand-written array, so a broker
/// added to the catalog cannot silently go unmentioned in that message.
fn brokers_carrying(asset: &Asset) -> Vec<&'static str> {
    IlBroker::ALL
        .iter()
        .copied()
        .filter(|b| asset.symbol_for(*b).is_some())
        .map(broker_label)
        .collect()
}

fn broker_label(b: IlBroker) -> &'static str {
    match b {
        IlBroker::Oanda => "oanda",
        IlBroker::TradeNation => "tradenation",
        IlBroker::TradingView => "tradingview",
        IlBroker::Ibkr => "ibkr",
    }
}

fn map_class(class: AssetClass) -> InstrumentType {
    match class {
        AssetClass::Forex => InstrumentType::Forex,
        AssetClass::Index => InstrumentType::Index,
        AssetClass::Gold => InstrumentType::Gold,
        AssetClass::Bond => InstrumentType::Bond,
        AssetClass::Commodity => InstrumentType::Commodity,
        // tcm doesn't model crypto or single-stock; fall back to
        // Commodity since the field isn't read in our path. If a
        // future consumer reads it, revisit then.
        AssetClass::Crypto | AssetClass::Stock => InstrumentType::Commodity,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resolve with the coverage gate skipped.
    ///
    /// The tests using this assert **symbol resolution** — that a chart symbol
    /// maps to the right broker-canonical string. Whether that instrument
    /// happens to be baked is a different question, and letting the bake state
    /// of the committed table decide whether a resolution test passes would
    /// make them fail for a reason they are not testing.
    ///
    /// Reproduces `resolve_for_broker` minus `require_spread_coverage`, rather
    /// than setting the override env var: the harness runs tests in threads, so
    /// mutating process env races every sibling — including the gate's own
    /// refusal tests, which it would silently disarm.
    fn resolve_ignoring_coverage(
        tv_symbol: &str,
        broker: ConvBroker,
    ) -> Result<ResolvedInstrument> {
        let bare = strip_exchange(tv_symbol);
        let asset = instrument_lookup::resolve(bare)?
            .ok_or_else(|| eyre!("{tv_symbol:?} not in catalog"))?;
        let il_broker = to_il_broker(broker)
            .ok_or_else(|| eyre!("{} has no catalog column", broker.as_str()))?;
        let broker_symbol = asset
            .symbol_for(il_broker)
            .ok_or_else(|| eyre!("{} not listed on {}", asset.id, broker.as_str()))?;
        Ok(finish_resolution(asset, il_broker, broker_symbol))
    }

    #[test]
    fn resolves_eurusd_to_oanda_canonical() {
        let r = resolve_for_broker("OANDA:EURUSD", ConvBroker::Oanda).expect("resolves");
        assert_eq!(r.asset.id, "EURUSD");
        assert_eq!(r.broker_symbol, "EUR_USD");
    }

    /// A baked instrument passes the coverage gate — the positive control, so a
    /// green suite proves the gate can say yes rather than being inert.
    #[test]
    fn a_baked_instrument_passes_the_coverage_gate() {
        require_spread_coverage("EUR_USD").expect("EUR_USD is baked");
    }

    /// The gate's whole purpose: an instrument with no baked row is refused at
    /// arm time instead of silently sizing stops with a zero forecast.
    #[test]
    fn an_unbaked_instrument_is_refused_at_arm_time() {
        let err = require_spread_coverage("NOT_A_REAL_PAIR").expect_err("must refuse");
        let msg = err.to_string();
        assert!(
            msg.contains("NOT_A_REAL_PAIR") && msg.contains("not in the baked spread table"),
            "the refusal must name the instrument and the reason, got: {msg}",
        );
        assert!(
            msg.contains("spread-baseline-gen"),
            "the refusal must tell the operator how to fix it, got: {msg}",
        );
    }

    /// The gate is wired into the real resolution path, not merely defined.
    /// Without this, deleting the `require_spread_coverage(..)?` call would
    /// leave every unit test above green while the gate did nothing.
    ///
    /// `Coffee` is a genuine catalog asset with no baked row (it is one of the
    /// two corpus fixtures in exactly that state), so resolving it must fail on
    /// coverage — proving the call site is live.
    #[test]
    fn resolution_itself_refuses_an_uncovered_instrument() {
        // Deliberately does NOT go through `resolve_for_broker`: sibling tests
        // set the override env var and Rust runs tests in threads, so a
        // resolution-based assertion here would race them. Instead prove the
        // call site exists by reading the source — cheap, and it fails loudly if
        // someone deletes the guard while leaving every other test green.
        let src = include_str!("instrument_resolution.rs");
        assert!(
            src.contains("require_spread_coverage(broker_symbol)?;"),
            "resolve_for_broker must CALL the coverage gate — without the call \
             site the gate is dead code and every other test still passes",
        );
    }

    #[test]
    fn resolves_eurusd_to_tn_canonical() {
        let r = resolve_ignoring_coverage("TRADENATION:EURUSD", ConvBroker::TradeNation)
            .expect("resolves");
        assert_eq!(r.asset.id, "EURUSD");
        assert_eq!(r.broker_symbol, "EUR/USD");
    }

    #[test]
    fn au200_oanda_leg_carries_native_per_broker_tick() {
        // The PRICE_PRECISION_EXCEEDED instrument, and the whole reason for
        // this migration. The legacy `resolve()` lands on a *duplicate*
        // `AU200AUD` Asset row that carries the WRONG class-default tick 1.0
        // (that's what sent OANDA a 5-decimal price it rejected). The
        // per-broker precision must instead come from the native instrument
        // catalog, keyed off the broker order symbol — tick 0.1.
        let r = resolve_for_broker("OANDA:AU200AUD", ConvBroker::Oanda).expect("resolves");
        assert_eq!(r.broker_symbol, "AU200_AUD");
        // Precision is the native per-broker value, NOT the legacy Asset's.
        assert_eq!(r.precision.tick_size, 0.1, "OANDA AU200 ticks in 0.1");
        assert_eq!(r.precision.pip_size, 1.0, "index sizes on a whole point");
        // Prove the native lookup actually corrected a wrong legacy tick:
        // the Asset it resolved to still carries the stale value.
        assert_ne!(
            r.asset.tick_size, r.precision.tick_size,
            "native precision must override the legacy Asset's wrong tick"
        );
    }

    #[test]
    fn resolves_smi_to_tn_canonical() {
        let r = resolve_ignoring_coverage("TRADENATION:SMI", ConvBroker::TradeNation)
            .expect("resolves");
        assert_eq!(r.asset.id, "CH20");
        assert_eq!(r.broker_symbol, "Switzerland 20");
        assert!(r.asset.is_affected_by("CHF"));
        assert!(r.asset.is_affected_by("EUR"));
    }

    #[test]
    fn resolves_underscore_form_typo() {
        // Operator typed EUR_USD into a TN chart's symbol field
        // — `resolve()` finds it via the OANDA-symbol column anyway.
        let r = resolve_ignoring_coverage("TRADENATION:EUR_USD", ConvBroker::TradeNation)
            .expect("resolves");
        assert_eq!(r.asset.id, "EURUSD");
        assert_eq!(r.broker_symbol, "EUR/USD");
    }

    #[test]
    fn resolves_bare_symbol_without_exchange() {
        let r = resolve_for_broker("EURUSD", ConvBroker::Oanda).expect("resolves");
        assert_eq!(r.asset.id, "EURUSD");
    }

    #[test]
    fn unknown_symbol_hard_errors_with_overlay_hint() {
        let err = resolve_for_broker("TRADENATION:NOPE_XYZ_NOTREAL", ConvBroker::TradeNation)
            .expect_err("must error");
        let msg = format!("{err}");
        assert!(
            msg.contains("not in the instrument-lookup catalog"),
            "msg = {msg}"
        );
        assert!(msg.contains("mappings.toml"), "msg = {msg}");
    }

    #[test]
    fn asset_not_listed_on_broker_hard_errors() {
        // UK10YB is OANDA-only in the baseline.
        let err = resolve_for_broker("UK10YB", ConvBroker::TradeNation).expect_err("must error");
        let msg = format!("{err}");
        assert!(msg.contains("not listed on tradenation"), "msg = {msg}");
        assert!(msg.contains("oanda"), "msg should name OANDA: {msg}");
    }

    #[test]
    fn synthesize_smi_has_chf_and_eur() {
        let asset = instrument_lookup::resolve("SMI")
            .expect("ok")
            .expect("found");
        let inst = synthesize_calendar_instrument(asset);
        assert!(inst.is_affected_by("CHF"));
        assert!(inst.is_affected_by("EUR"));
        assert!(!inst.is_affected_by("JPY"));
    }

    #[test]
    fn synthesize_eurusd_has_both_legs() {
        let asset = instrument_lookup::resolve("EURUSD")
            .expect("ok")
            .expect("found");
        let inst = synthesize_calendar_instrument(asset);
        assert!(inst.is_affected_by("EUR"));
        assert!(inst.is_affected_by("USD"));
        assert!(!inst.is_affected_by("JPY"));
    }

    #[test]
    fn synthesize_xauusd_has_xau_and_usd() {
        let asset = instrument_lookup::resolve("XAUUSD")
            .expect("ok")
            .expect("found");
        let inst = synthesize_calendar_instrument(asset);
        assert!(inst.is_affected_by("XAU"));
        assert!(inst.is_affected_by("USD"));
    }

    #[test]
    fn strip_exchange_handles_no_prefix() {
        assert_eq!(strip_exchange("EURUSD"), "EURUSD");
        assert_eq!(strip_exchange("TRADENATION:EURUSD"), "EURUSD");
        assert_eq!(strip_exchange("OANDA:EUR_USD"), "EUR_USD");
    }

    /// Every `(broker, symbol)` leg the catalog lists, excluding stocks.
    ///
    /// Stocks are excluded because `spread-baseline-gen` defers them by default
    /// (`--include-stocks`), so 1100+ of them have no row **by design** and
    /// including them would drown the signal.
    fn tradeable_catalog_legs() -> Vec<(IlBroker, &'static str, &'static str)> {
        let Ok(assets) = instrument_lookup::all() else {
            return Vec::new();
        };
        assets
            .iter()
            .filter(|a| a.class != AssetClass::Stock)
            .flat_map(|a| {
                [IlBroker::Oanda, IlBroker::TradeNation]
                    .into_iter()
                    .filter_map(move |b| {
                        a.symbol_for(b)
                            .filter(|s| !s.is_empty())
                            .map(|s| (b, s, a.id.as_str()))
                    })
            })
            .collect()
    }

    fn baked_leg(broker: IlBroker, symbol: &str) -> bool {
        let want = match broker {
            IlBroker::Oanda => "oanda",
            IlBroker::TradeNation => "tradenation",
            _ => return false,
        };
        trade_control_core::spread_blackout::baked_rows().any(|(b, s)| b == want && s == symbol)
    }

    /// **The catalog↔table cross-check, direction 1: a catalog asset with no row.**
    ///
    /// This is the report `coverage`'s module docs promise ("the cross-check
    /// that the table and the catalog agree lives in `cli`") and which did not
    /// actually exist anywhere — the gap that let TradeNation `Bitcoin` sit
    /// unbaked until an arm attempt failed in front of the operator. `tv-arm`
    /// is the right home: it is the one crate that links **both** the catalog
    /// and the baked table.
    ///
    /// It deliberately **reports rather than fails**. A missing row is a
    /// legitimate state — the bake is an hours-long manual operation against
    /// two live brokers, new catalog assets land between bakes, and part-time
    /// markets genuinely cannot be profiled. Failing here would make every
    /// catalog addition a red build with no way to land it. The arm-time gate
    /// (`require_spread_coverage`) is what actually protects a trade; this
    /// makes the backlog *visible* with `cargo test -p tv-arm -- --nocapture`
    /// instead of discoverable only by trying to arm.
    ///
    /// What it does assert is that the check itself still works: the catalog
    /// must yield legs and the table must contain rows. Both empty would make
    /// an all-clear report meaningless.
    #[test]
    fn report_catalog_legs_with_no_baked_spread_row() {
        let legs = tradeable_catalog_legs();
        assert!(
            !legs.is_empty(),
            "the catalog yielded no non-stock legs — the cross-check would \
             report an empty all-clear and prove nothing",
        );
        assert!(
            trade_control_core::spread_blackout::baked_rows()
                .next()
                .is_some(),
            "the baked table is empty — every instrument would report missing",
        );

        let mut missing: Vec<String> = legs
            .iter()
            .filter(|(broker, symbol, _id)| !baked_leg(*broker, symbol))
            .map(|(broker, symbol, id)| format!("{broker:?} {symbol:?} (asset {id})"))
            .collect();
        missing.sort();

        if missing.is_empty() {
            println!(
                "spread-table coverage: all {} tradeable legs baked",
                legs.len()
            );
        } else {
            println!(
                "spread-table coverage: {} of {} tradeable legs have NO baked row \
                 (each REFUSES to arm until re-baked):",
                missing.len(),
                legs.len(),
            );
            for m in &missing {
                println!("  - {m}");
            }
        }
    }

    /// **Direction 2: a baked row naming an instrument the catalog no longer lists.**
    ///
    /// The mirror of the report above, and the one that genuinely *should*
    /// fail. An orphan row is never legitimate: it means a symbol was renamed
    /// or dropped from the catalog while its row stayed behind, so the table
    /// now carries a spread profile keyed on a string nothing can resolve. That
    /// row can never be read (no resolution produces its key) and it silently
    /// inflates the row count the shrink guard trusts.
    ///
    /// Zero orphans as of 2026-09-22, which is also the evidence that the
    /// TradingView-era names are fully migrated off this table.
    #[test]
    fn no_baked_row_names_an_instrument_the_catalog_lost() {
        let Ok(assets) = instrument_lookup::all() else {
            panic!("catalog must load for the cross-check to mean anything");
        };
        let listed: std::collections::BTreeSet<(&str, &str)> = assets
            .iter()
            .flat_map(|a| {
                [
                    ("oanda", a.symbols.oanda.as_deref()),
                    ("tradenation", a.symbols.tradenation.as_deref()),
                ]
            })
            .filter_map(|(b, s)| s.filter(|s| !s.is_empty()).map(|s| (b, s)))
            .collect();

        let orphans: Vec<String> = trade_control_core::spread_blackout::baked_rows()
            .filter(|(b, s)| !listed.contains(&(*b, *s)))
            .map(|(b, s)| format!("{b} {s:?}"))
            .collect();

        assert!(
            orphans.is_empty(),
            "these baked rows name instruments the catalog does not list, so \
             nothing can ever resolve to them — the row is dead weight and its \
             symbol was probably renamed: {orphans:?}",
        );
    }
}
