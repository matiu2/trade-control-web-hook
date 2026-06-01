//! Recovery path for chart symbols that aren't in the
//! instrument-lookup catalog.
//!
//! When [`crate::instrument_resolution::resolve_for_broker`] fails
//! because the chart's TV symbol (e.g. `"GOOGL"`) doesn't match any
//! catalog asset, we ask tv-mcp for the chart's symbol-info
//! ([`SymbolInfo`]). Its `description` field carries the broker's own
//! name for the asset (e.g. `"ALPHABET"` for `TRADENATION:GOOGL`),
//! which usually does match — TradeNation's column of the catalog
//! uses those names verbatim.
//!
//! On a successful recovery we patch the user overlay
//! (`~/.config/instrument-lookup/mappings.toml`) so the missing
//! `tradingview = "<sym>"` mapping is recorded, then re-load the
//! catalog so the rest of the run sees the patched asset. Next run
//! skips this whole path.
//!
//! On a miss-then-miss we surface a hard error that includes the
//! exact TOML block the operator can paste — far more actionable than
//! the original "symbol not in catalog" message.

use color_eyre::eyre::{Result, WrapErr, eyre};
use instrument_lookup::{Asset, Overlay};
use trading_view::symbol_info::SymbolInfo;

/// One catalog entry, patched (or freshly created) with the chart's
/// TV symbol. Returned by [`build_patched_asset`] so the call site
/// can `upsert` + `save` it through [`Overlay`].
#[derive(Debug, Clone)]
pub struct PatchedAsset {
    /// The asset to upsert into the overlay. If we matched an
    /// existing baseline entry, this is a clone of it with
    /// `symbols.tradingview = Some(tv_symbol)`. If no match was
    /// found, the caller falls back to the snippet hint instead.
    pub asset: Asset,
    /// Whether the asset already existed in the catalog (true) or
    /// is a brand-new entry (false). Logged so the operator can see
    /// what the recovery actually did.
    pub already_existed: bool,
}

/// Try to find the asset that the chart's `info.description` refers
/// to, then return a [`PatchedAsset`] with the TV symbol filled in.
///
/// Returns `None` when neither `description`, `symbol`, nor
/// `full_name` resolves — the caller should error with a snippet
/// hint (see [`overlay_snippet_hint`]).
///
/// This function is pure: it consults the in-memory catalog but
/// performs no file I/O. The caller is responsible for the
/// `Overlay::load → upsert → save` sequence.
pub fn build_patched_asset(info: &SymbolInfo) -> Result<Option<PatchedAsset>> {
    // Try description first (TN's column matches this), then the
    // bare symbol, then full_name. resolve() is liberal — slash form,
    // underscore form, display name all work.
    let candidates = [
        info.description.as_str(),
        info.symbol.as_str(),
        info.full_name.as_str(),
    ];
    for candidate in candidates {
        if candidate.is_empty() {
            continue;
        }
        if let Some(asset) = instrument_lookup::resolve(candidate)
            .wrap_err_with(|| format!("catalog resolve of {candidate:?} during recovery"))?
        {
            let mut patched = asset.clone();
            patched.symbols.tradingview = Some(info.symbol.clone());
            return Ok(Some(PatchedAsset {
                asset: patched,
                already_existed: true,
            }));
        }
    }
    Ok(None)
}

/// Build a copy-pasteable TOML block the operator can drop into
/// `~/.config/instrument-lookup/mappings.toml` when automatic
/// recovery can't find a match.
///
/// The block fills in everything tv-arm can infer from `info`
/// (`tradingview` symbol, class from `info.asset_type`, display name
/// from `description`) and leaves the broker symbols blank so the
/// operator only has to fill in the one their chart points at.
pub fn overlay_snippet_hint(info: &SymbolInfo) -> String {
    let id_guess = canonicalize_id(&info.description, &info.symbol);
    let class = map_tv_type(&info.asset_type);
    format!(
        "[[asset]]\n\
         id = \"{id_guess}\"\n\
         class = \"{class}\"\n\
         display_name = \"{desc}\"\n\
         description = \"{desc}\"\n\
         news_currencies = []  # fill in: e.g. [\"USD\"] for a US stock\n\
         oanda = \"\"\n\
         tradenation = \"\"\n\
         tradingview = \"{tv}\"",
        id_guess = id_guess,
        class = class,
        desc = info.description,
        tv = info.symbol,
    )
}

/// Canonical-id guess: uppercase, alphanumeric only. Falls back to
/// the symbol if description is empty. The operator can rename it.
fn canonicalize_id(description: &str, fallback: &str) -> String {
    let source = if description.is_empty() {
        fallback
    } else {
        description
    };
    let id: String = source
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_uppercase();
    if id.is_empty() {
        fallback.to_uppercase()
    } else {
        id
    }
}

/// Map TV's `type` string onto the catalog's `class` enum value.
/// Unknown types fall back to `"commodity"` (same fallback used
/// inside `instrument_resolution::map_class` for stocks/crypto).
fn map_tv_type(tv_type: &str) -> &'static str {
    match tv_type {
        "forex" => "forex",
        "index" => "index",
        "stock" => "stock",
        "crypto" => "crypto",
        "bond" => "bond",
        "gold" => "gold",
        _ => "commodity",
    }
}

/// Persist a [`PatchedAsset`] into the user overlay. Returns the
/// overlay path on success so the caller can log it.
///
/// Splits load/upsert/validate/save into one call so the caller
/// doesn't have to know about the overlay machinery directly.
pub fn save_patch(patched: &PatchedAsset) -> Result<std::path::PathBuf> {
    let path = instrument_lookup::user_config_path()
        .ok_or_else(|| eyre!("can't determine user overlay path (HOME unset?)"))?;
    let mut overlay = Overlay::load(path.clone())
        .wrap_err_with(|| format!("loading overlay at {}", path.display()))?;
    overlay.upsert(&patched.asset);
    overlay
        .validate_with_baseline()
        .wrap_err("patched overlay failed validation — refusing to save")?;
    overlay.save().wrap_err("saving patched overlay")?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn googl_info() -> SymbolInfo {
        SymbolInfo {
            symbol: "GOOGL".into(),
            full_name: "TRADENATION:GOOGL".into(),
            exchange: "Trade Nation".into(),
            description: "ALPHABET".into(),
            asset_type: "stock".into(),
        }
    }

    #[test]
    fn recovers_alphabet_via_description() {
        let info = googl_info();
        let patched = build_patched_asset(&info)
            .expect("ok")
            .expect("alphabet is in the baseline");
        assert!(patched.already_existed);
        assert_eq!(patched.asset.id, "ALPHABET");
        assert_eq!(patched.asset.symbols.tradingview.as_deref(), Some("GOOGL"));
        // TN symbol carries over unchanged from the baseline.
        assert_eq!(
            patched.asset.symbols.tradenation.as_deref(),
            Some("ALPHABET")
        );
    }

    #[test]
    fn returns_none_when_nothing_resolves() {
        let info = SymbolInfo {
            symbol: "TOTALLYFAKE_XYZ".into(),
            full_name: "FAKEEX:TOTALLYFAKE_XYZ".into(),
            exchange: "Fake".into(),
            description: "DEFINITELY_NOT_A_REAL_ASSET_QQQ".into(),
            asset_type: "stock".into(),
        };
        let out = build_patched_asset(&info).expect("ok");
        assert!(out.is_none(), "should not have resolved: {out:?}");
    }

    #[test]
    fn snippet_hint_is_well_formed_toml() {
        let info = googl_info();
        let snippet = overlay_snippet_hint(&info);
        // Spot-check the obvious fields land in the right place.
        assert!(snippet.contains("[[asset]]"));
        assert!(snippet.contains("id = \"ALPHABET\""));
        assert!(snippet.contains("class = \"stock\""));
        assert!(snippet.contains("tradingview = \"GOOGL\""));
        // Round-trip it through toml so we know it parses.
        let parsed: toml::Value = toml::from_str(&snippet).expect("snippet must parse as TOML");
        assert!(parsed.get("asset").and_then(|v| v.as_array()).is_some());
    }

    #[test]
    fn canonicalize_id_strips_non_alnum() {
        assert_eq!(
            canonicalize_id("Trade Nation 500", "TN500"),
            "TRADENATION500"
        );
        assert_eq!(canonicalize_id("EUR/USD", "EURUSD"), "EURUSD");
        assert_eq!(canonicalize_id("", "FOOBAR"), "FOOBAR");
        // All-punctuation description falls back to the symbol.
        assert_eq!(canonicalize_id("///", "FOOBAR"), "FOOBAR");
    }

    #[test]
    fn map_tv_type_falls_back_to_commodity() {
        assert_eq!(map_tv_type("stock"), "stock");
        assert_eq!(map_tv_type("forex"), "forex");
        assert_eq!(map_tv_type("something-unexpected"), "commodity");
        assert_eq!(map_tv_type(""), "commodity");
    }
}
