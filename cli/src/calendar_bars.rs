//! `calendar-bars` — auto-emit pause + news alert pairs for upcoming
//! economic-calendar events affecting a trade's instrument.
//!
//! Bridges [`trade_calendar_maker`] / [`forex_factory`] to the existing
//! [`pause_pattern`] and [`news_pattern`] builders. For each qualifying
//! event in the look-ahead window, splits the event's blocking buffer
//! into two halves at `event_time`:
//!
//!   - **pause window**: `[event_time − buffer_before, event_time]` —
//!     blocks new entries during the lead-up to the event.
//!   - **news window**:  `[event_time, event_time + buffer_after]` —
//!     arms reversal-close on already-open trades during the event
//!     and its immediate aftermath.
//!
//! Same pause and news intents the operator could draw manually; just
//! sourced from the calendar instead. Each event gets a deterministic
//! id `cal-<currency-lower>-<event-slug>-<event-epoch>-{pause,news}`,
//! so re-running is idempotent and calendar-drawn windows never collide
//! with operator-drawn ones (KV partitions on the id).
//!
//! [`pause_pattern`]: crate::pause_pattern
//! [`news_pattern`]: crate::news_pattern

use std::path::PathBuf;

use chrono::{DateTime, Local, Utc};
use clap::{Parser, ValueEnum};
use color_eyre::eyre::{Context, Result, eyre};
use forex_factory::{EconomicEvent, Impact};
use trade_calendar_maker::{Instrument, Timeframe};
use trade_control_core::intent::BrokerKind;
use trade_control_core::sig::KEY_LEN;

use crate::forex_factory_cache::get_week_events_cached;
use crate::news_pattern::{NewsSpec, build_news_from_spec, write_news};
use crate::pause_pattern::{PauseSpec, build_pause_from_spec, write_pause};

/// One calendar-derived row: original event metadata, plus the two
/// specs that the I/O layer will hand to `build_pause_from_spec` and
/// `build_news_from_spec`. Pure planner output — no signing, no disk.
#[derive(Debug, Clone)]
pub struct CalendarBarRow {
    /// Stable slug for this event, used as the per-event subdirectory
    /// name in the output layout. Format `<currency-lower>-<name-slug>-<epoch>`.
    pub event_slug: String,
    pub event_name: String,
    pub currency: String,
    pub impact: Impact,
    pub event_time: DateTime<Utc>,
    pub pause_spec: PauseSpec,
    pub news_spec: NewsSpec,
}

/// The planner's output. Rows are sorted by `event_time` ascending so
/// the summary table renders in chronological order.
#[derive(Debug, Clone)]
pub struct CalendarBarPlan {
    pub rows: Vec<CalendarBarRow>,
}

/// Inputs the planner needs that aren't on the calendar event itself.
/// Mirrors the `args` half of `CalendarBarsArgs` but stays free of clap
/// types so unit tests can construct it directly without `parse_from`.
#[derive(Debug, Clone)]
pub struct PlanInputs {
    pub trade_id: String,
    pub instrument: String,
    pub account: String,
    pub broker: BrokerKind,
}

/// Plan calendar bars: filter events by lookahead window, impact, and
/// instrument-affected-by-currency, then split each kept event into a
/// pause-spec and a news-spec. Pure — no I/O, no signing — so tests can
/// hand it a fixture `Vec<EconomicEvent>` and assert on the result.
pub fn plan_calendar_bars(
    events: &[EconomicEvent],
    instrument: &Instrument,
    timeframe: Timeframe,
    now: DateTime<Utc>,
    inputs: &PlanInputs,
) -> Result<CalendarBarPlan> {
    let min_impact = timeframe.min_blocking_impact();
    let buf_before = timeframe.buffer_before();
    let buf_after = timeframe.buffer_after();
    let lookahead_end = now + buf_before + buf_after;

    let mut rows: Vec<CalendarBarRow> = Vec::new();
    for ev in events {
        let event_utc = ev.datetime.with_timezone(&Utc);
        if event_utc <= now {
            continue;
        }
        if event_utc > lookahead_end {
            continue;
        }
        if ev.impact < min_impact {
            continue;
        }
        if !instrument.is_affected_by(&ev.currency) {
            continue;
        }

        let name_slug = slugify(&ev.name);
        let event_slug = format!(
            "{}-{}-{}",
            ev.currency.to_lowercase(),
            name_slug,
            event_utc.timestamp()
        );
        let reason_pause = format!("cal-{}-{}-pause", ev.currency.to_uppercase(), name_slug);
        let reason_news = format!("cal-{}-{}-news", ev.currency.to_uppercase(), name_slug);

        let pause_spec = PauseSpec {
            trade_id: inputs.trade_id.clone(),
            blackout_id: Some(format!("cal-{event_slug}-pause")),
            start_time: event_utc - buf_before,
            end_time: event_utc,
            reason: Some(reason_pause),
            instrument: inputs.instrument.clone(),
            account: inputs.account.clone(),
            broker: inputs.broker,
        };
        let news_spec = NewsSpec {
            trade_id: inputs.trade_id.clone(),
            news_id: Some(format!("cal-{event_slug}-news")),
            start_time: event_utc,
            end_time: event_utc + buf_after,
            reason: Some(reason_news),
            instrument: inputs.instrument.clone(),
            account: inputs.account.clone(),
            broker: inputs.broker,
        };

        rows.push(CalendarBarRow {
            event_slug,
            event_name: ev.name.clone(),
            currency: ev.currency.clone(),
            impact: ev.impact,
            event_time: event_utc,
            pause_spec,
            news_spec,
        });
    }
    rows.sort_by_key(|r| r.event_time);
    Ok(CalendarBarPlan { rows })
}

/// Lowercase, replace runs of non-`[a-z0-9]` with a single `-`, trim
/// leading/trailing hyphens. Matches the slug shape `is_valid_trade_id`
/// enforces in the worker (lowercase alphanumerics + hyphens, no
/// leading/trailing/consecutive hyphens). Truncates to 48 chars so the
/// combined `cal-<currency>-<slug>-<epoch>-pause` fits the worker's
/// 64-char id limit for `blackout_id` / `news_id`.
fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_hyphen = true;
    for c in s.chars() {
        let lower = c.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            out.push(lower);
            prev_hyphen = false;
        } else if !prev_hyphen {
            out.push('-');
            prev_hyphen = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.len() > 48 {
        out.truncate(48);
        while out.ends_with('-') {
            out.pop();
        }
    }
    if out.is_empty() {
        return "event".to_string();
    }
    out
}

/// Parse the CLI's broker-facing instrument symbol (e.g. `EUR_USD`)
/// into a [`trade_calendar_maker::Instrument`]. Strips the underscore
/// OANDA forex pairs carry — `from_oanda_symbol` expects the bare
/// concatenated form (`EURUSD`).
pub fn parse_instrument(raw: &str) -> Result<Instrument> {
    let normalised = raw.replace('_', "");
    Instrument::from_oanda_symbol(&normalised)
        .ok_or_else(|| eyre!("unsupported instrument symbol {raw:?}"))
}

/// Clap-side mirror of [`trade_calendar_maker::Timeframe`]. Lives here
/// so the CLI binary doesn't have to depend on `trade-calendar-maker`
/// directly just to enum-derive a flag.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum TimeframeArg {
    /// 15-minute charts: 2-star+ events within 3 hours.
    M15,
    /// 1-hour+ charts: 3-star events within 8 hours.
    H1plus,
}

impl From<TimeframeArg> for Timeframe {
    fn from(v: TimeframeArg) -> Self {
        match v {
            TimeframeArg::M15 => Timeframe::M15,
            TimeframeArg::H1plus => Timeframe::H1Plus,
        }
    }
}

/// Clap-side mirror of [`BrokerKind`]. Re-declared here so the module
/// is self-contained — the binary's own `BrokerKindArg` is private to
/// `trade_control.rs` and we don't want to make it public just for this.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum CalendarBrokerArg {
    Oanda,
    TradeNation,
}

impl From<CalendarBrokerArg> for BrokerKind {
    fn from(v: CalendarBrokerArg) -> Self {
        match v {
            CalendarBrokerArg::Oanda => BrokerKind::Oanda,
            CalendarBrokerArg::TradeNation => BrokerKind::TradeNation,
        }
    }
}

#[derive(Parser, Debug)]
pub struct CalendarBarsArgs {
    /// Instrument the trade is on, in the broker's native form (e.g.
    /// `EUR_USD`). Underscore-stripped to match
    /// [`Instrument::from_oanda_symbol`].
    #[arg(long)]
    pub instrument: String,
    /// Parent trade the auto-drawn bars apply to. Must match the
    /// `trade_id` of the `05-enter` alert the operator is arming —
    /// pause + news KV entries are partitioned by trade_id.
    #[arg(long)]
    pub trade_id: String,
    /// Account name from the local history cache. Validated the same
    /// way `build-pause` / `build-news` validate theirs.
    #[arg(long)]
    pub account: String,
    /// Broker the parent trade targets. Stamped onto every emitted
    /// alert; defaults to OANDA to match the typical demo flow.
    #[arg(long, value_enum, default_value_t = CalendarBrokerArg::Oanda)]
    pub broker: CalendarBrokerArg,
    /// TradingView chart timeframe — picks both the impact threshold
    /// and the look-ahead/buffer windows. M15 = 2-star+ within 3h;
    /// H1plus = 3-star only within 8h. See `trade-calendar-maker`'s
    /// own `Timeframe` for the source-of-truth values.
    #[arg(long, value_enum)]
    pub timeframe: TimeframeArg,
    /// Path to a hex-encoded 32-byte signing key. Same key the other
    /// `build-*` paths use — calendar-bars alerts go through the same
    /// HMAC pipeline as manually-drawn ones.
    #[arg(long, env = "TRADE_CONTROL_KEY_FILE")]
    pub key_file: PathBuf,
    /// Directory to write emitted YAMLs under. Created if missing.
    /// Default: `./calendar-bars/<trade_id>/`. Each event becomes a
    /// `<event_slug>/{pause,news}/` subtree so operators can prune
    /// per-event.
    #[arg(long)]
    pub output_dir: Option<PathBuf>,
    /// Print the plan to stdout but write nothing. Useful for previewing
    /// what bars the calendar would arm before committing them.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
}

/// Async wrapper around `forex_factory::CalendarService::get_week_events_for`,
/// routed through the disk cache at `~/.cache/tv-arm/forex-factory/`
/// (see [`crate::forex_factory_cache`]). Splits I/O from the pure
/// planner so callers can mock events in tests.
pub async fn fetch_week_events(now: DateTime<Utc>) -> Result<Vec<EconomicEvent>> {
    let local_today = now.with_timezone(&Local).date_naive();
    get_week_events_cached(local_today).await
}

/// Fetch every forex-factory event whose timestamp falls in `[from, to]`,
/// walking the weeks the range spans. Each fetch is a separate HTTP
/// round-trip; consecutive fetches may return overlapping events when
/// the range straddles a week boundary, so the result is run through
/// [`dedupe_and_filter_events`] before being returned.
///
/// Used by `tv-news` to align the events it annotates with the chart's
/// visible window — typically 2.5–3 weeks. Bounded to 10 weeks to
/// catch operator misuse (e.g. accidentally fetching a whole year's
/// worth of calendar pages).
pub async fn fetch_events_for_range(
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<EconomicEvent>> {
    if to < from {
        return Err(eyre!(
            "fetch_events_for_range: `to` ({to}) is before `from` ({from})"
        ));
    }
    let anchors = week_anchors(from, to);
    if anchors.len() > 10 {
        return Err(eyre!(
            "fetch_events_for_range: range spans {} weeks, more than the 10-week guard rail",
            anchors.len(),
        ));
    }

    let mut all = Vec::new();
    for anchor in anchors {
        let week = get_week_events_cached(anchor).await?;
        all.extend(week);
    }
    Ok(dedupe_and_filter_events(all, from, to))
}

/// The set of dates to pass to `get_week_events_for` so that every week
/// overlapping `[from, to]` is fetched exactly once.
///
/// Anchors are Mondays (UTC) — forex-factory's `week=YYYYMMDD` URL
/// parameter picks the week the date falls in, so any day in that week
/// works, but Mondays make the dedupe step's "fetched twice in
/// overlapping calls" property easy to eyeball in logs.
fn week_anchors(from: DateTime<Utc>, to: DateTime<Utc>) -> Vec<chrono::NaiveDate> {
    use chrono::{Datelike, Duration, Weekday};

    let mut day = from.date_naive();
    // Walk back to the Monday of `from`'s week.
    let from_weekday = day.weekday().num_days_from_monday() as i64;
    day -= Duration::days(from_weekday);

    let to_day = to.date_naive();
    let mut out = Vec::new();
    while day <= to_day {
        out.push(day);
        day += Duration::days(7);
        // Defensive: bail if we somehow overshoot — `to` can be at most
        // a few weeks past `from`, so this never fires in practice.
        if out.len() > 60 {
            break;
        }
        let _ = Weekday::Mon; // silence unused-import lint if needed
    }
    out
}

/// Deduplicate `events` by `(datetime, name, currency)` and retain only
/// those whose timestamp falls inside `[from, to]`. Pure — every effect
/// is in the input.
///
/// Order is preserved (first-seen wins) so callers can rely on the
/// upstream forex-factory ordering for stable per-week chronology.
pub fn dedupe_and_filter_events(
    events: Vec<EconomicEvent>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Vec<EconomicEvent> {
    use std::collections::HashSet;
    let mut seen: HashSet<(i64, String, String)> = HashSet::new();
    let mut out = Vec::with_capacity(events.len());
    for ev in events {
        let event_utc = ev.datetime.with_timezone(&Utc);
        if event_utc < from || event_utc > to {
            continue;
        }
        let key = (event_utc.timestamp(), ev.name.clone(), ev.currency.clone());
        if seen.insert(key) {
            out.push(ev);
        }
    }
    out
}

/// Pretty-print the plan as a one-event-per-row summary. Same shape as
/// the per-alert lines `build-pause` / `build-news` print — operators
/// (and `tv_arm_hs.py`, eventually) can parse the per-event header to
/// locate each output dir.
pub fn print_summary_table(plan: &CalendarBarPlan) {
    if plan.rows.is_empty() {
        println!("no qualifying events in window");
        return;
    }
    println!("event_time              currency  impact   event");
    for row in &plan.rows {
        println!(
            "{:23}  {:8}  {:6}   {}",
            row.event_time.to_rfc3339(),
            row.currency,
            format!("{:?}", row.impact),
            row.event_name,
        );
    }
}

/// Sync entry point for the binary. Builds its own multi-thread tokio
/// runtime for the single async fetch — keeps the rest of the CLI sync
/// and avoids forcing every other subcommand into `#[tokio::main]`.
pub fn run_calendar_bars(
    args: CalendarBarsArgs,
    key: [u8; KEY_LEN],
    now: DateTime<Utc>,
) -> Result<()> {
    let instrument = parse_instrument(&args.instrument)?;
    let timeframe: Timeframe = args.timeframe.into();
    let inputs = PlanInputs {
        trade_id: args.trade_id.clone(),
        instrument: args.instrument.clone(),
        account: args.account.clone(),
        broker: args.broker.into(),
    };

    let runtime = tokio::runtime::Runtime::new().context("starting tokio runtime")?;
    let events = runtime.block_on(fetch_week_events(now))?;
    let plan = plan_calendar_bars(&events, &instrument, timeframe, now, &inputs)?;

    println!("trade_id: {}", args.trade_id);
    println!("instrument: {}", args.instrument);
    println!("timeframe: {timeframe}");
    println!("events_fetched: {}", events.len());
    println!("events_kept: {}", plan.rows.len());
    print_summary_table(&plan);

    if args.dry_run {
        println!("(dry-run — no files written)");
        return Ok(());
    }
    if plan.rows.is_empty() {
        return Ok(());
    }

    let out_root = args
        .output_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("calendar-bars").join(&args.trade_id));
    println!("output: {}", out_root.display());

    for row in &plan.rows {
        let event_dir = out_root.join(&row.event_slug);
        let built_pause = build_pause_from_spec(row.pause_spec.clone(), now)
            .with_context(|| format!("building pause for {}", row.event_slug))?;
        let written_pause = write_pause(&built_pause, &key, &event_dir.join("pause"))?;
        let built_news = build_news_from_spec(row.news_spec.clone(), now)
            .with_context(|| format!("building news for {}", row.event_slug))?;
        let written_news = write_news(&built_news, &key, &event_dir.join("news"))?;
        println!(
            "  - {} → pause: {}, news: {}",
            row.event_slug,
            written_pause.display(),
            written_news.display(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone};
    use trade_calendar_maker::InstrumentType;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn ev(name: &str, currency: &str, impact: Impact, time_utc: &str) -> EconomicEvent {
        EconomicEvent {
            name: name.to_string(),
            currency: currency.to_string(),
            impact,
            datetime: Local.from_utc_datetime(&ts(time_utc).naive_utc()),
            actual: None,
            forecast: None,
            previous: None,
        }
    }

    fn eur_usd() -> Instrument {
        Instrument {
            symbol: "EURUSD".to_string(),
            instrument_type: InstrumentType::Forex,
            affected_currencies: vec!["EUR".to_string(), "USD".to_string()],
        }
    }

    fn inputs() -> PlanInputs {
        PlanInputs {
            trade_id: "eurusd-hs-1".to_string(),
            instrument: "EUR_USD".to_string(),
            account: "oanda-reversals-demo".to_string(),
            broker: BrokerKind::Oanda,
        }
    }

    #[test]
    fn drops_events_in_the_past() {
        let now = ts("2026-06-06T12:00:00Z");
        let events = vec![
            ev("Past CPI", "USD", Impact::High, "2026-06-06T11:30:00Z"),
            ev("Future NFP", "USD", Impact::High, "2026-06-06T13:30:00Z"),
        ];
        let plan = plan_calendar_bars(&events, &eur_usd(), Timeframe::M15, now, &inputs()).unwrap();
        assert_eq!(plan.rows.len(), 1);
        assert_eq!(plan.rows[0].event_name, "Future NFP");
    }

    #[test]
    fn drops_events_beyond_lookahead() {
        let now = ts("2026-06-06T12:00:00Z");
        // M15 lookahead = 3h + 1h = 4h. 4h05m out → dropped.
        let events = vec![
            ev("Within", "USD", Impact::High, "2026-06-06T15:30:00Z"),
            ev("Beyond", "USD", Impact::High, "2026-06-06T16:05:00Z"),
        ];
        let plan = plan_calendar_bars(&events, &eur_usd(), Timeframe::M15, now, &inputs()).unwrap();
        assert_eq!(plan.rows.len(), 1);
        assert_eq!(plan.rows[0].event_name, "Within");
    }

    #[test]
    fn m15_drops_low_keeps_medium_and_high() {
        let now = ts("2026-06-06T12:00:00Z");
        let events = vec![
            ev("Low evt", "USD", Impact::Low, "2026-06-06T13:00:00Z"),
            ev("Med evt", "USD", Impact::Medium, "2026-06-06T13:10:00Z"),
            ev("High evt", "USD", Impact::High, "2026-06-06T13:20:00Z"),
        ];
        let plan = plan_calendar_bars(&events, &eur_usd(), Timeframe::M15, now, &inputs()).unwrap();
        let kept: Vec<&str> = plan.rows.iter().map(|r| r.event_name.as_str()).collect();
        assert_eq!(kept, vec!["Med evt", "High evt"]);
    }

    #[test]
    fn h1plus_drops_low_and_medium_keeps_only_high() {
        let now = ts("2026-06-06T00:00:00Z");
        // H1Plus lookahead = 8h + 1h. All events within window.
        let events = vec![
            ev("Low evt", "USD", Impact::Low, "2026-06-06T01:00:00Z"),
            ev("Med evt", "USD", Impact::Medium, "2026-06-06T02:00:00Z"),
            ev("High evt", "USD", Impact::High, "2026-06-06T03:00:00Z"),
        ];
        let plan =
            plan_calendar_bars(&events, &eur_usd(), Timeframe::H1Plus, now, &inputs()).unwrap();
        let kept: Vec<&str> = plan.rows.iter().map(|r| r.event_name.as_str()).collect();
        assert_eq!(kept, vec!["High evt"]);
    }

    #[test]
    fn drops_events_for_unaffected_currency() {
        let now = ts("2026-06-06T12:00:00Z");
        let events = vec![
            ev("USD evt", "USD", Impact::High, "2026-06-06T13:00:00Z"),
            ev("JPY evt", "JPY", Impact::High, "2026-06-06T13:10:00Z"),
            ev("EUR evt", "EUR", Impact::High, "2026-06-06T13:20:00Z"),
        ];
        let plan = plan_calendar_bars(&events, &eur_usd(), Timeframe::M15, now, &inputs()).unwrap();
        let kept: Vec<&str> = plan.rows.iter().map(|r| r.event_name.as_str()).collect();
        assert_eq!(kept, vec!["USD evt", "EUR evt"]);
    }

    #[test]
    fn splits_window_at_event_time() {
        let now = ts("2026-06-06T12:00:00Z");
        let event_t = ts("2026-06-06T13:30:00Z");
        let events = vec![ev("NFP", "USD", Impact::High, "2026-06-06T13:30:00Z")];
        let plan = plan_calendar_bars(&events, &eur_usd(), Timeframe::M15, now, &inputs()).unwrap();
        let row = &plan.rows[0];
        // Pause runs from event - 3h to event.
        assert_eq!(
            row.pause_spec.start_time,
            event_t - chrono::Duration::hours(3)
        );
        assert_eq!(row.pause_spec.end_time, event_t);
        // News runs from event to event + 1h.
        assert_eq!(row.news_spec.start_time, event_t);
        assert_eq!(row.news_spec.end_time, event_t + chrono::Duration::hours(1));
        // The two halves abut exactly at event_time.
        assert_eq!(row.pause_spec.end_time, row.news_spec.start_time);
    }

    #[test]
    fn ids_are_deterministic_and_slug_valid() {
        let now = ts("2026-06-06T12:00:00Z");
        let events = vec![ev(
            "Non-Farm Employment Change",
            "USD",
            Impact::High,
            "2026-06-06T13:30:00Z",
        )];
        let plan = plan_calendar_bars(&events, &eur_usd(), Timeframe::M15, now, &inputs()).unwrap();
        let row = &plan.rows[0];
        let blackout_id = row.pause_spec.blackout_id.as_deref().unwrap();
        let news_id = row.news_spec.news_id.as_deref().unwrap();
        assert!(
            trade_control_core::intent::is_valid_trade_id(blackout_id),
            "blackout_id not a valid slug: {blackout_id:?}"
        );
        assert!(
            trade_control_core::intent::is_valid_trade_id(news_id),
            "news_id not a valid slug: {news_id:?}"
        );
        // Replaying with the same event yields the same ids — idempotent.
        let plan2 =
            plan_calendar_bars(&events, &eur_usd(), Timeframe::M15, now, &inputs()).unwrap();
        assert_eq!(
            plan.rows[0].pause_spec.blackout_id,
            plan2.rows[0].pause_spec.blackout_id
        );
        assert_eq!(
            plan.rows[0].news_spec.news_id,
            plan2.rows[0].news_spec.news_id
        );
    }

    #[test]
    fn rows_sorted_by_event_time() {
        let now = ts("2026-06-06T12:00:00Z");
        let events = vec![
            ev("Later", "USD", Impact::High, "2026-06-06T15:00:00Z"),
            ev("Earlier", "USD", Impact::High, "2026-06-06T13:30:00Z"),
            ev("Middle", "USD", Impact::High, "2026-06-06T14:15:00Z"),
        ];
        let plan = plan_calendar_bars(&events, &eur_usd(), Timeframe::M15, now, &inputs()).unwrap();
        let times: Vec<_> = plan.rows.iter().map(|r| r.event_name.clone()).collect();
        assert_eq!(times, vec!["Earlier", "Middle", "Later"]);
    }

    #[test]
    fn parse_instrument_strips_underscore() {
        let inst = parse_instrument("EUR_USD").unwrap();
        assert!(inst.is_affected_by("EUR"));
        assert!(inst.is_affected_by("USD"));
        assert!(!inst.is_affected_by("JPY"));
    }

    #[test]
    fn parse_instrument_rejects_garbage() {
        let err = parse_instrument("NOT_A_THING").unwrap_err();
        assert!(err.to_string().contains("unsupported"), "{err}");
    }

    #[test]
    fn slugify_handles_punctuation_and_runs() {
        assert_eq!(
            slugify("Non-Farm Employment Change"),
            "non-farm-employment-change"
        );
        assert_eq!(slugify("CPI m/m"), "cpi-m-m");
        assert_eq!(slugify("   --leading--   "), "leading");
        assert_eq!(slugify(""), "event");
    }

    #[test]
    fn week_anchors_walks_back_to_monday() {
        // 2026-06-06 was a Saturday. Range Sat → Sun (next week) should
        // anchor on the Mondays of both weeks.
        let from = ts("2026-06-06T12:00:00Z"); // Sat
        let to = ts("2026-06-14T12:00:00Z"); // next-week Sun
        let anchors = week_anchors(from, to);
        assert_eq!(
            anchors,
            vec![
                chrono::NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
                chrono::NaiveDate::from_ymd_opt(2026, 6, 8).unwrap(),
            ]
        );
    }

    #[test]
    fn week_anchors_handles_three_week_range() {
        // Typical tv-news window — Sun in week-1 to Sat in week-3.
        let from = ts("2026-06-07T00:00:00Z"); // Sun (week 22-anchor: 2026-06-01)
        let to = ts("2026-06-27T23:59:59Z"); // Sat (week 24-anchor: 2026-06-22)
        let anchors = week_anchors(from, to);
        // Should hit Mondays 2026-06-01, 2026-06-08, 2026-06-15, 2026-06-22.
        assert_eq!(anchors.len(), 4);
        assert_eq!(
            anchors.first().copied(),
            Some(chrono::NaiveDate::from_ymd_opt(2026, 6, 1).unwrap())
        );
        assert_eq!(
            anchors.last().copied(),
            Some(chrono::NaiveDate::from_ymd_opt(2026, 6, 22).unwrap())
        );
    }

    #[test]
    fn dedupe_and_filter_drops_out_of_range() {
        let from = ts("2026-06-06T00:00:00Z");
        let to = ts("2026-06-13T00:00:00Z");
        let events = vec![
            ev("Before", "USD", Impact::High, "2026-06-05T12:00:00Z"),
            ev("In range", "USD", Impact::High, "2026-06-10T12:00:00Z"),
            ev("After", "USD", Impact::High, "2026-06-14T12:00:00Z"),
        ];
        let kept = dedupe_and_filter_events(events, from, to);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].name, "In range");
    }

    #[test]
    fn dedupe_collapses_repeats_in_overlapping_fetches() {
        // Simulate two weekly fetches whose results overlap: same event
        // appearing twice when both weeks include 2026-06-10.
        let from = ts("2026-06-06T00:00:00Z");
        let to = ts("2026-06-20T00:00:00Z");
        let same = ev("CPI m/m", "USD", Impact::High, "2026-06-10T12:30:00Z");
        let events = vec![
            same.clone(),
            ev("NFP", "USD", Impact::High, "2026-06-11T12:30:00Z"),
            same, // duplicate (would come from the next week's page)
        ];
        let kept = dedupe_and_filter_events(events, from, to);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].name, "CPI m/m");
        assert_eq!(kept[1].name, "NFP");
    }

    #[test]
    fn dedupe_distinguishes_same_name_different_currency() {
        let from = ts("2026-06-06T00:00:00Z");
        let to = ts("2026-06-20T00:00:00Z");
        let events = vec![
            ev("CPI m/m", "USD", Impact::High, "2026-06-10T12:30:00Z"),
            ev("CPI m/m", "EUR", Impact::High, "2026-06-10T12:30:00Z"),
        ];
        let kept = dedupe_and_filter_events(events, from, to);
        assert_eq!(kept.len(), 2);
    }
}
