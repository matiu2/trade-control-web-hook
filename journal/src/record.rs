//! Persisting a journalled trade to the per-environment SQLite journal so its
//! outcome can be queried later (win-rate / R / expectancy by entry-mode,
//! instrument class, broker, …).
//!
//! One row per `trade_id`, upserted — re-recording a plan overwrites its row.
//! The **real-life** outcome (from the live `plan timeline`) and the **replay**
//! outcome (from the `replay-candles` report) are stored in separate columns so
//! a later query can compare them or slice on either.
//!
//! Storage is a single file per environment at
//! `~/.config/trade-control/journal-<env>.db` (the suffix mirrors the CLIs).
//! Querying is deliberately out of scope for now — record here, query with
//! `sqlite3` by hand.

use std::path::PathBuf;

use color_eyre::eyre::{Result, eyre};
use rusqlite::Connection;

use crate::divergence::{ReplayOutcome, parse_replay_outcome};
use crate::plan::{EntryMode, PlanDetail};
use crate::timeline::{derive_entry_ts, derive_outcome};

/// The env suffix baked at compile time (mirrors `cli::ENV_SUFFIX`). Kept here
/// too so the DB filename is env-scoped without cross-module coupling.
const ENV_SUFFIX: &str = env!("BAKED_ENV_SUFFIX");

/// One journalled trade: identity + dimensions + the two separate outcomes.
/// All the fields are already-derived facts from the plan sources — this struct
/// is just their durable, queryable shape.
#[derive(Debug, Clone, PartialEq)]
pub struct TradeRecord {
    // Identity / query dimensions.
    pub trade_id: String,
    pub instrument: String,
    pub instrument_class: String,
    pub broker: String,
    pub direction: String,
    pub granularity: String,
    pub entry_mode: String,
    pub order_type: String,
    pub armed_at: Option<String>,
    pub recorded_at: String,

    // Real-life outcome (live `plan timeline`).
    pub live_entry_ts: Option<String>,
    pub live_outcome: String,
    pub live_is_ok: bool,

    // Replay outcome (`replay-candles` report).
    pub replay_done: Option<bool>,
    pub replay_final_phase: Option<String>,
    pub replay_fires: Option<i64>,
    pub replay_tp: Option<i64>,
    pub replay_sl: Option<i64>,
    pub replay_net_r: Option<String>,
}

impl TradeRecord {
    /// Assemble a record from the already-parsed plan sources. `detail` supplies
    /// the identity + dimensions, `timeline_json` the real-life outcome,
    /// `replay_report` the replay outcome. `recorded_at` is passed in (the caller
    /// stamps `now`) so this stays pure/testable.
    pub fn from_plan(
        detail: &PlanDetail,
        timeline_json: &str,
        replay_report: &str,
        recorded_at: String,
    ) -> Self {
        let (live_outcome, live_is_ok) = derive_outcome(timeline_json);
        let live_entry_ts = derive_entry_ts(timeline_json);
        let ReplayOutcome {
            done,
            final_phase,
            fires,
            tp,
            sl,
            net_r,
        } = parse_replay_outcome(replay_report);

        Self {
            trade_id: detail.trade_id.clone(),
            instrument: detail.instrument.clone(),
            instrument_class: instrument_class(&detail.instrument),
            broker: detail.broker.clone(),
            direction: detail.direction.clone(),
            granularity: detail.granularity.clone(),
            entry_mode: entry_mode_key(detail.entry_mode).to_string(),
            order_type: detail
                .order_types
                .first()
                .map(|(_, ot)| ot.label().to_string())
                .unwrap_or_else(|| "?".to_string()),
            armed_at: detail.armed_at.clone(),
            recorded_at,
            live_entry_ts,
            live_outcome,
            live_is_ok,
            replay_done: done,
            replay_final_phase: final_phase,
            replay_fires: fires.map(|n| n as i64),
            replay_tp: tp.map(|n| n as i64),
            replay_sl: sl.map(|n| n as i64),
            replay_net_r: net_r,
        }
    }
}

/// A short, stable key for an entry mode (the DB query dimension). Distinct from
/// `EntryMode::label` (which is human prose with parentheses).
fn entry_mode_key(mode: EntryMode) -> &'static str {
    match mode {
        EntryMode::Normal => "normal",
        EntryMode::Quasimodo => "quasimodo",
        EntryMode::StrategyV2 => "strategy-v2",
        EntryMode::Unknown => "unknown",
    }
}

/// The instrument's asset class (forex / index / gold / …) via `instrument-lookup`,
/// so "win-rate on indexes" is a real query. Tries both broker views of the raw
/// id; `"unknown"` if the asset isn't in the catalog.
fn instrument_class(instrument: &str) -> String {
    use instrument_lookup::{Broker, by_broker_symbol};
    [Broker::Oanda, Broker::TradeNation]
        .into_iter()
        .find_map(|b| {
            by_broker_symbol(b, instrument)
                .ok()
                .flatten()
                // `AssetClass` renders lowercase via Display (forex/index/gold/…).
                .map(|asset| asset.class.to_string())
        })
        .unwrap_or_else(|| "unknown".to_string())
}

/// The per-environment journal DB path: `~/.config/trade-control/journal-<env>.db`
/// (bare `journal.db` when built with no env suffix).
pub fn db_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    let name = if ENV_SUFFIX.is_empty() {
        "journal.db".to_string()
    } else {
        format!("journal-{ENV_SUFFIX}.db")
    };
    PathBuf::from(home).join(".config/trade-control").join(name)
}

/// Open (creating if needed) the journal DB at `path` and migrate its schema.
/// The migration is idempotent (`CREATE TABLE IF NOT EXISTS`).
pub fn open_db(path: &std::path::Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| eyre!("create {}: {e}", dir.display()))?;
    }
    let conn = Connection::open(path).map_err(|e| eyre!("open journal db: {e}"))?;
    migrate(&conn)?;
    Ok(conn)
}

/// Create the `trades` table if it doesn't exist.
fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS trades (
            trade_id          TEXT PRIMARY KEY,
            instrument        TEXT NOT NULL,
            instrument_class  TEXT NOT NULL,
            broker            TEXT NOT NULL,
            direction         TEXT NOT NULL,
            granularity       TEXT NOT NULL,
            entry_mode        TEXT NOT NULL,
            order_type        TEXT NOT NULL,
            armed_at          TEXT,
            recorded_at       TEXT NOT NULL,
            live_entry_ts     TEXT,
            live_outcome      TEXT NOT NULL,
            live_is_ok        INTEGER NOT NULL,
            replay_done       INTEGER,
            replay_final_phase TEXT,
            replay_fires      INTEGER,
            replay_tp         INTEGER,
            replay_sl         INTEGER,
            replay_net_r      TEXT
        );",
    )
    .map_err(|e| eyre!("migrate journal db: {e}"))?;
    Ok(())
}

/// Migrate an already-open connection (test helper: lets the app tests point
/// `record_current` at an in-memory DB).
#[cfg(test)]
pub fn migrate_for_test(conn: &Connection) {
    migrate(conn).expect("migrate in-memory db");
}

/// Insert or replace the trade's row (keyed on `trade_id`), so re-recording a
/// plan updates it rather than erroring.
pub fn upsert(conn: &Connection, rec: &TradeRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO trades (
            trade_id, instrument, instrument_class, broker, direction,
            granularity, entry_mode, order_type, armed_at, recorded_at,
            live_entry_ts, live_outcome, live_is_ok,
            replay_done, replay_final_phase, replay_fires, replay_tp,
            replay_sl, replay_net_r
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
            ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19
        )
        ON CONFLICT(trade_id) DO UPDATE SET
            instrument=excluded.instrument,
            instrument_class=excluded.instrument_class,
            broker=excluded.broker,
            direction=excluded.direction,
            granularity=excluded.granularity,
            entry_mode=excluded.entry_mode,
            order_type=excluded.order_type,
            armed_at=excluded.armed_at,
            recorded_at=excluded.recorded_at,
            live_entry_ts=excluded.live_entry_ts,
            live_outcome=excluded.live_outcome,
            live_is_ok=excluded.live_is_ok,
            replay_done=excluded.replay_done,
            replay_final_phase=excluded.replay_final_phase,
            replay_fires=excluded.replay_fires,
            replay_tp=excluded.replay_tp,
            replay_sl=excluded.replay_sl,
            replay_net_r=excluded.replay_net_r",
        rusqlite::params![
            rec.trade_id,
            rec.instrument,
            rec.instrument_class,
            rec.broker,
            rec.direction,
            rec.granularity,
            rec.entry_mode,
            rec.order_type,
            rec.armed_at,
            rec.recorded_at,
            rec.live_entry_ts,
            rec.live_outcome,
            rec.live_is_ok as i64,
            rec.replay_done.map(|b| b as i64),
            rec.replay_final_phase,
            rec.replay_fires,
            rec.replay_tp,
            rec.replay_sl,
            rec.replay_net_r,
        ],
    )
    .map_err(|e| eyre!("upsert trade {}: {e}", rec.trade_id))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::parse_plan_export;

    const EXPORT: &str = include_str!("../tests/fixtures/plan_export.json");
    const TIMELINE: &str = include_str!("../tests/fixtures/plan_timeline.json");
    const REPLAY: &str = include_str!("../tests/fixtures/replay_report.txt");

    /// A record assembled from the AUD/CAD fixtures carries the right dimensions
    /// and both outcomes.
    #[test]
    fn builds_record_from_fixtures() {
        let detail = parse_plan_export(EXPORT).unwrap();
        let rec = TradeRecord::from_plan(
            &detail,
            TIMELINE,
            REPLAY,
            "2026-07-23T00:00:00Z".to_string(),
        );

        assert_eq!(rec.trade_id, "hs-aud-cad-a07622da");
        assert_eq!(rec.instrument, "AUD_CAD");
        // AUD/CAD is a forex cross in the instrument-lookup catalog.
        assert_eq!(rec.instrument_class, "forex");
        assert_eq!(rec.broker, "oanda");
        assert_eq!(rec.direction, "short");
        assert_eq!(rec.entry_mode, "normal");
        assert_eq!(rec.order_type, "stop");

        // Real-life outcome: this fixture has only dumps → the fallback text.
        assert_eq!(rec.live_outcome, "no dispatch recorded");
        assert!(!rec.live_is_ok);

        // Replay outcome comes from the summary line.
        assert_eq!(rec.replay_done, Some(false));
        assert_eq!(
            rec.replay_final_phase.as_deref(),
            Some("AwaitBreakAndClose")
        );
        assert_eq!(rec.replay_fires, Some(4));
        assert_eq!(rec.replay_tp, Some(0));
        assert_eq!(rec.replay_sl, Some(0));
        assert_eq!(rec.replay_net_r.as_deref(), Some("+0.00"));
    }

    /// Open an in-memory DB, upsert a record, read it back.
    #[test]
    fn upsert_then_read_back() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        let detail = parse_plan_export(EXPORT).unwrap();
        let rec = TradeRecord::from_plan(
            &detail,
            TIMELINE,
            REPLAY,
            "2026-07-23T00:00:00Z".to_string(),
        );
        upsert(&conn, &rec).unwrap();

        let (instrument, mode, net_r): (String, String, Option<String>) = conn
            .query_row(
                "SELECT instrument, entry_mode, replay_net_r FROM trades WHERE trade_id = ?1",
                [&rec.trade_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(instrument, "AUD_CAD");
        assert_eq!(mode, "normal");
        assert_eq!(net_r.as_deref(), Some("+0.00"));
    }

    /// A second upsert of the same trade_id overwrites, not duplicates.
    #[test]
    fn upsert_is_idempotent_by_trade_id() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        let detail = parse_plan_export(EXPORT).unwrap();
        let mut rec = TradeRecord::from_plan(
            &detail,
            TIMELINE,
            REPLAY,
            "2026-07-23T00:00:00Z".to_string(),
        );
        upsert(&conn, &rec).unwrap();

        // Record it again with a changed outcome — same trade_id.
        rec.replay_net_r = Some("+1.25".to_string());
        rec.recorded_at = "2026-07-24T00:00:00Z".to_string();
        upsert(&conn, &rec).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM trades", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "same trade_id upserts, never duplicates");

        let net_r: Option<String> = conn
            .query_row(
                "SELECT replay_net_r FROM trades WHERE trade_id = ?1",
                [&rec.trade_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(net_r.as_deref(), Some("+1.25"), "the row was updated");
    }

    #[test]
    fn entry_mode_keys_are_stable_slugs() {
        assert_eq!(entry_mode_key(EntryMode::Normal), "normal");
        assert_eq!(entry_mode_key(EntryMode::Quasimodo), "quasimodo");
        assert_eq!(entry_mode_key(EntryMode::StrategyV2), "strategy-v2");
        assert_eq!(entry_mode_key(EntryMode::Unknown), "unknown");
    }

    #[test]
    fn db_path_is_env_scoped() {
        let p = db_path();
        let name = p.file_name().unwrap().to_string_lossy();
        // Bare in a plain test build; suffixed under a deploy build.
        if ENV_SUFFIX.is_empty() {
            assert_eq!(name, "journal.db");
        } else {
            assert_eq!(name, format!("journal-{ENV_SUFFIX}.db"));
        }
    }
}
