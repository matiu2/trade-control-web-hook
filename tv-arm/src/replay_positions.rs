//! Draw a replay's positions onto local-chart.
//!
//! `replay-candles --positions <path>` writes **what happened**; this reads it
//! and paints it. The split is deliberate: the simulator does not know about
//! charts, so a chart backend can change (or be replaced) without touching the
//! code that decides what a trade did.
//!
//! ## Why the colours live here and not in the simulator
//!
//! Green/red/grey and the transparency levels are *presentation*. They belong
//! to whoever is drawing, not to the replay that computed the trade — the same
//! reason the positions file carries a `taken` flag and an `outcome` string
//! rather than a colour.
//!
//! ## Stable ids, and why there is no sidecar manifest
//!
//! The TradingView path (`replay-candles`' `annotate.rs`) tracks its drawings
//! in `~/.config/trade-control/replay-annotations.json`, a file of TradingView
//! entity-ids, because TV's position tool has **no writable id**: the only way
//! to clean up a prior run is to remember what it created.
//!
//! local-chart upserts by *the caller's own id*, so this path derives a
//! deterministic one per position ([`drawing_id`]). Re-running replaces each
//! drawing in place instead of accumulating duplicates, and no state has to
//! survive between runs. That is also why the manifest must never be shared
//! between the two: it holds TradingView ids, and asking local-chart to delete
//! them (or vice versa) would be a no-op at best.

use color_eyre::eyre::{Result, WrapErr};
use local_chart_client::{ChartKey, DrawingsClient, Position, Side, local_chart_symbol};
use serde::Deserialize;
use tracing::{info, warn};

/// Long tint (TradingView's default long-green, kept so the two backends look
/// the same during the changeover).
const LONG_COLOR: &str = "#26a69a";
/// Short tint (TradingView's default short-red).
const SHORT_COLOR: &str = "#ef5350";
/// Muted tint for a *not-taken* trade — grey, so the operator can tell at a
/// glance that it never went on.
const UNFILLED_COLOR: &str = "#787b86";
/// Zone transparency for taken positions (0 opaque … 100 invisible). Light
/// tint so the candles underneath stay readable.
const ZONE_TRANSPARENCY: u8 = 80;
/// Fainter still for a not-taken trade — it is only the *intended* bracket.
const UNFILLED_TRANSPARENCY: u8 = 90;

/// Prefix every drawing this path creates carries, so a prior run's positions
/// can be found and cleared without a manifest. Distinctive enough not to
/// collide with anything the operator draws by hand.
const ID_PREFIX: &str = "replay-pos";

/// The `--positions` document, as `replay-candles` writes it.
///
/// Its own `Deserialize` rather than a shared type: this is a wire contract
/// between two binaries, and the parsing side should fail loudly when the
/// producing side changes shape. `deny_unknown_fields` is deliberately NOT
/// set — a newer `replay-candles` adding a field must not break an older
/// `tv-arm`; the `version` check below is what guards an incompatible change.
#[derive(Debug, Clone, Deserialize)]
pub struct PositionsFile {
    pub version: u32,
    pub instrument: String,
    pub granularity: String,
    pub positions: Vec<PositionRow>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PositionRow {
    pub direction: String,
    pub fill_at: i64,
    pub until: i64,
    pub entry_price: f64,
    pub stop_loss: f64,
    pub take_profit: f64,
    pub outcome: String,
    pub taken: bool,
}

/// The `version` this code understands. A file claiming a newer one is
/// **refused**, not parsed optimistically: the fields it would silently read
/// wrong are price levels, and a bracket drawn at the wrong levels looks
/// entirely plausible.
const SUPPORTED_VERSION: u32 = 1;

/// A deterministic drawing id for one position.
///
/// Keyed on the **fill time and direction**, which together identify a
/// position within a chart: a replay cannot open two positions the same way on
/// the same bar. Deterministic so a re-run upserts in place — see the module
/// docs on why that removes the need for a sidecar manifest.
fn drawing_id(row: &PositionRow) -> String {
    format!("{ID_PREFIX}-{}-{}", row.direction, row.fill_at)
}

/// The (tint, transparency) for a position. A *taken* trade gets its
/// directional green/red; a *not-taken* one gets muted grey, fainter still, so
/// it reads as "intended, never went on".
fn style(row: &PositionRow) -> (&'static str, u8) {
    if !row.taken {
        return (UNFILLED_COLOR, UNFILLED_TRANSPARENCY);
    }
    let color = if row.direction == "long" {
        LONG_COLOR
    } else {
        SHORT_COLOR
    };
    (color, ZONE_TRANSPARENCY)
}

/// Project one row onto a local-chart position drawing.
///
/// Returns `None` for a direction this does not recognise rather than guessing
/// a side. A guessed direction would draw a coherent bracket for the *opposite*
/// trade — the failure mode that loses money on a glance.
fn to_position(row: &PositionRow) -> Option<Position> {
    let side = match row.direction.as_str() {
        "long" => Side::Long,
        "short" => Side::Short,
        _ => return None,
    };
    let (color, transparency) = style(row);
    Some(Position {
        id: drawing_id(row),
        time1: row.fill_at,
        time2: row.until,
        entry: row.entry_price,
        stop_loss: row.stop_loss,
        take_profit: row.take_profit,
        side,
        color: color.to_string(),
        transparency,
    })
}

/// Read a positions file written by `replay-candles --positions`.
pub fn read(path: &std::path::Path) -> Result<PositionsFile> {
    let text = std::fs::read_to_string(path)
        .wrap_err_with(|| format!("reading positions from {}", path.display()))?;
    let doc: PositionsFile = serde_json::from_str(&text)
        .wrap_err_with(|| format!("parsing positions from {}", path.display()))?;
    if doc.version != SUPPORTED_VERSION {
        return Err(color_eyre::eyre::eyre!(
            "positions file is version {} but this tv-arm understands {SUPPORTED_VERSION} — \
             rebuild both from the same checkout rather than drawing levels that may have \
             changed meaning",
            doc.version
        ));
    }
    Ok(doc)
}

/// Draw every position in `doc` onto the local-chart at `base_url`.
///
/// Clears the drawings a prior run of THIS path left on the same chart first,
/// found by id prefix — so a re-replay does not leave a previous run's
/// brackets behind. Only ids carrying [`ID_PREFIX`] are touched: the
/// operator's hand-drawn necklines, fibs and H&S anchors are never at risk,
/// which is the guarantee that makes running this on a working chart safe.
///
/// Returns how many positions were drawn.
pub fn draw(doc: &PositionsFile, base_url: &str, include_unfilled: bool) -> Result<usize> {
    let client = DrawingsClient::new(base_url)?;
    let key = ChartKey::new(local_chart_symbol(&doc.instrument), &doc.granularity);

    let cleared = clear_prior(&client, &key)?;
    if cleared > 0 {
        info!(cleared, "removed prior replay positions");
    }

    let wanted = doc.positions.iter().filter(|r| include_unfilled || r.taken);
    let mut drawn = 0usize;
    for row in wanted {
        let Some(position) = to_position(row) else {
            warn!(
                direction = %row.direction,
                "skipping a position with an unrecognised direction — refusing to guess a side"
            );
            continue;
        };
        client.upsert(&key, &position.to_drawing())?;
        drawn += 1;
        info!(
            outcome = %row.outcome,
            direction = %row.direction,
            taken = row.taken,
            "drew position"
        );
    }
    Ok(drawn)
}

/// Remove the drawings a prior run of this path left, identified by their id
/// prefix. Ids the operator deleted by hand are already gone and are skipped
/// without error.
fn clear_prior(client: &DrawingsClient, key: &ChartKey) -> Result<usize> {
    let existing = client.list(key)?;
    let ours: Vec<String> = existing
        .iter()
        .filter_map(|d| d.get("id").and_then(|v| v.as_str()))
        .filter(|id| id.starts_with(ID_PREFIX))
        .map(str::to_string)
        .collect();

    let mut removed = 0usize;
    for id in ours {
        if client.remove(key, &id)? {
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(direction: &str, taken: bool) -> PositionRow {
        PositionRow {
            direction: direction.to_string(),
            fill_at: 1_700_000_000,
            until: 1_700_086_400,
            entry_price: 1.2000,
            stop_loss: 1.1950,
            take_profit: 1.2100,
            outcome: "tp".to_string(),
            taken,
        }
    }

    /// The property that replaces the sidecar manifest: the same position must
    /// produce the same id every run, so a redraw upserts in place.
    #[test]
    fn the_same_position_gets_the_same_id_every_run() {
        assert_eq!(
            drawing_id(&row("long", true)),
            drawing_id(&row("long", true))
        );
    }

    /// Two positions on one chart must not collide, or the second would
    /// overwrite the first and one bracket would silently vanish.
    #[test]
    fn different_positions_get_different_ids() {
        let mut later = row("long", true);
        later.fill_at += 3600;
        assert_ne!(drawing_id(&row("long", true)), drawing_id(&later));
        assert_ne!(
            drawing_id(&row("long", true)),
            drawing_id(&row("short", true)),
            "a long and a short on the same bar are different positions"
        );
    }

    /// Every id must carry the prefix `clear_prior` searches for. An id
    /// without it would never be cleaned up, and each replay would leave
    /// another orphaned bracket on the chart.
    #[test]
    fn every_id_carries_the_prefix_the_cleanup_searches_for() {
        for d in ["long", "short"] {
            assert!(
                drawing_id(&row(d, true)).starts_with(ID_PREFIX),
                "{d} id must be findable by prefix"
            );
        }
    }

    #[test]
    fn taken_positions_are_tinted_by_direction_and_not_taken_are_muted() {
        assert_eq!(style(&row("long", true)), (LONG_COLOR, ZONE_TRANSPARENCY));
        assert_eq!(style(&row("short", true)), (SHORT_COLOR, ZONE_TRANSPARENCY));

        let (color, transparency) = style(&row("long", false));
        assert_eq!(color, UNFILLED_COLOR, "not-taken is muted");
        assert!(
            transparency > ZONE_TRANSPARENCY,
            "not-taken is fainter than taken"
        );
    }

    /// Direction comes from the row's own word, never from which side the stop
    /// sits on. A short whose stop is above entry must stay a short.
    #[test]
    fn direction_is_read_from_the_row_not_inferred_from_the_levels() {
        let mut short = row("short", true);
        short.stop_loss = 1.2050;
        short.take_profit = 1.1900;
        let pos = to_position(&short).expect("a short projects");
        assert_eq!(pos.side, Side::Short);
        assert_eq!(pos.stop_loss, 1.2050, "levels are carried as given");
    }

    /// An unrecognised direction must be REFUSED, not guessed. Defaulting to
    /// long would draw a coherent-looking bracket for the opposite trade.
    #[test]
    fn an_unknown_direction_is_refused_rather_than_guessed() {
        assert!(to_position(&row("sideways", true)).is_none());
        assert!(to_position(&row("", true)).is_none());
        assert!(to_position(&row("LONG", true)).is_none(), "case is exact");
    }

    #[test]
    fn the_absolute_levels_reach_the_drawing_untouched() {
        let pos = to_position(&row("long", true)).expect("projects");
        assert_eq!(pos.entry, 1.2000);
        assert_eq!(pos.stop_loss, 1.1950);
        assert_eq!(pos.take_profit, 1.2100);
        assert_eq!(pos.time1, 1_700_000_000);
        assert_eq!(pos.time2, 1_700_086_400);
    }

    /// A version this code does not understand must be refused. The fields it
    /// would misread are price levels, and a bracket at the wrong levels looks
    /// entirely plausible on a chart.
    #[test]
    fn a_future_version_is_refused_rather_than_parsed_optimistically() {
        let dir = std::env::temp_dir().join(format!("tv-arm-pos-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("future.json");
        std::fs::write(
            &path,
            r#"{"version":999,"instrument":"EUR_USD","granularity":"h1","positions":[]}"#,
        )
        .expect("write");

        let err = read(&path).expect_err("a future version must not be accepted");
        assert!(
            err.to_string().contains("999"),
            "the error names the version it saw: {err}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_current_version_file_parses() {
        let dir = std::env::temp_dir().join(format!("tv-arm-pos-ok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("positions.json");
        std::fs::write(
            &path,
            r#"{"version":1,"instrument":"EUR_USD","granularity":"h1","positions":[
                {"direction":"long","fill_at":1700000000,"until":1700086400,
                 "entry_price":1.2,"stop_loss":1.195,"take_profit":1.21,
                 "exit_price":1.21,"outcome":"tp","taken":true}]}"#,
        )
        .expect("write");

        let doc = read(&path).expect("parses");
        assert_eq!(doc.version, SUPPORTED_VERSION);
        assert_eq!(doc.instrument, "EUR_USD");
        assert_eq!(doc.positions.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- against a REAL local-chart server ------------------------------
    //
    // These call the SAME `read` / `draw` the binary calls — the point is to
    // exercise the real code path, which a test outside this module could not
    // do (tv-arm is bin-only, so nothing can `use` it).
    //
    // `#[ignore]`d: they need a server.
    //
    //   mkdir -p /tmp/lcc2/data
    //   XDG_DATA_HOME=/tmp/lcc2/data local-chart --port 8813 &
    //   LOCAL_CHART_TEST_URL=http://127.0.0.1:8813 cargo test -- --ignored
    //
    // ⚠️ 8790/8787/8788 hold the operator's REAL drawings and these WRITE, so
    // they refuse those ports rather than trusting the variable.

    const FORBIDDEN_PORTS: &[&str] = &[":8790", ":8787", ":8788"];

    fn test_url() -> Option<String> {
        let url = std::env::var("LOCAL_CHART_TEST_URL").ok()?;
        assert!(
            !FORBIDDEN_PORTS.iter().any(|p| url.contains(p)),
            "REFUSING to write to {url} — that is a live server holding real \
             drawings. Use a scratch port with its own XDG_DATA_HOME."
        );
        Some(url)
    }

    fn live_doc() -> PositionsFile {
        PositionsFile {
            version: SUPPORTED_VERSION,
            instrument: "EUR_USD".to_string(),
            granularity: "h1".to_string(),
            positions: vec![row("long", true), {
                let mut short = row("short", false);
                short.fill_at = 1_700_100_000;
                short.until = 1_700_186_400;
                short
            }],
        }
    }

    fn our_ids(client: &DrawingsClient, key: &ChartKey) -> Vec<String> {
        let mut ids: Vec<String> = client
            .list(key)
            .expect("listing")
            .iter()
            .filter_map(|d| d.get("id").and_then(|v| v.as_str()))
            .filter(|id| id.starts_with(ID_PREFIX))
            .map(str::to_string)
            .collect();
        ids.sort();
        ids
    }

    /// The load-bearing live test: positions land, a re-run REPLACES rather
    /// than accumulates, and the operator's own drawings are never touched.
    ///
    /// The last part is what makes running a replay on a working chart safe —
    /// and it is the guarantee that would break silently if `clear_prior` ever
    /// stopped filtering by prefix.
    #[test]
    #[ignore = "needs a running local-chart; see the notes above"]
    fn positions_land_a_rerun_replaces_them_and_the_operator_is_untouched() {
        let Some(url) = test_url() else {
            eprintln!("skipped: set LOCAL_CHART_TEST_URL");
            return;
        };
        let doc = live_doc();
        let client = DrawingsClient::new(&url).expect("client builds");
        let key = ChartKey::new(local_chart_symbol(&doc.instrument), &doc.granularity);

        // A drawing the OPERATOR made, which must survive everything below.
        let theirs = serde_json::json!({
            "id": "operator-neckline",
            "type": "horizontal-line",
            "anchors": [{ "time": 1_700_000_000, "price": 1.5 }],
        });
        client.upsert(&key, &theirs).expect("seeding");

        let drawn = draw(&doc, &url, true).expect("first run draws");
        assert_eq!(drawn, 2, "taken AND not-taken drawn when asked for both");
        let first = our_ids(&client, &key);
        assert_eq!(first.len(), 2, "two of our drawings: {first:?}");

        let drawn_again = draw(&doc, &url, true).expect("second run draws");
        assert_eq!(drawn_again, 2);
        let second = our_ids(&client, &key);
        assert_eq!(
            second.len(),
            2,
            "a re-run REPLACES rather than piling up: {second:?}"
        );
        assert_eq!(
            first, second,
            "ids are stable across runs — what makes the upsert land in place, \
             and why this path needs no sidecar manifest"
        );

        let all = client.list(&key).expect("listing");
        assert!(
            all.iter().any(|d| d["id"] == "operator-neckline"),
            "the operator's own drawing survived both runs"
        );

        for id in our_ids(&client, &key) {
            client.remove(&key, &id).ok();
        }
        client.remove(&key, "operator-neckline").ok();
    }

    /// Without `include_unfilled`, only the taken positions are drawn — the
    /// not-taken ones are an *intended* bracket the operator may not want.
    #[test]
    #[ignore = "needs a running local-chart; see the notes above"]
    fn only_taken_positions_are_drawn_when_unfilled_are_excluded() {
        let Some(url) = test_url() else {
            eprintln!("skipped: set LOCAL_CHART_TEST_URL");
            return;
        };
        let doc = live_doc();
        let client = DrawingsClient::new(&url).expect("client builds");
        let key = ChartKey::new(local_chart_symbol(&doc.instrument), &doc.granularity);

        let drawn = draw(&doc, &url, false).expect("draws");
        assert_eq!(drawn, 1, "only the taken position");
        let ids = our_ids(&client, &key);
        assert_eq!(ids.len(), 1, "and only one reached the chart: {ids:?}");
        assert!(
            ids[0].contains("long"),
            "the taken one is the long: {ids:?}"
        );

        for id in our_ids(&client, &key) {
            client.remove(&key, &id).ok();
        }
    }
}
