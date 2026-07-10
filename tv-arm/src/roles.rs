//! Group chart drawings by the role the operator has assigned them.
//!
//! Port of `tv_arm_hs.py`'s `classify()` (lines ~189–263). The
//! operator labels each drawing with one of the vocabularies in
//! [`trade_control_conventions::labels`]; `classify()` walks the
//! drawings, dispatches by `(kind, label)`, and returns a [`Roles`]
//! struct ready for the alert pipeline.
//!
//! When multiple drawings claim the same single-slot role
//! (invalidation, neckline, retest, tp fib, trade-expiry), resolution is
//! two steps. **First, the visible-window filter** drops any drawing whose
//! time-span lies *entirely* outside the chart's on-screen window — in
//! *both* run modes. This is the structural fix for the recurring "armed
//! against the wrong pattern" bug: a stale off-screen drawing (e.g. a
//! neckline drawn weeks after the setup) can no longer out-rank the in-view
//! one just by being newer. Intersection (not containment) is used, so a
//! line spanning the whole view or poking past one edge survives; the
//! trade-expiry marker additionally keeps a small forward margin past the
//! right edge, where it's *meant* to sit.
//!
//! **Second, among the survivors, the tiebreak** depends on the run mode
//! ([`SlotPref`]): live arming (`--register-plan`) keeps the *latest* one —
//! older drawings are stale leftovers — while an offline / replay build
//! (`--plan-out` alone) prefers the drawing belonging to the on-screen
//! window. Dropped counts are logged at `info`, and a genuinely ambiguous
//! chart (more than one drawing left in-window for a single-slot role) is
//! logged at `warn` so the operator can clean up the clutter.
//!
//! Blackout and news windows are **not** collected here. They are resolved from
//! the economic calendar at real event-minute precision by the pipeline's
//! `calendar_windows` step (see [`crate::news_window`]) and pushed into
//! `Roles::blackout_pairs` / `news_pairs` after `classify`. Drawn pause/resume
//! /news vertical lines left on a chart are ignored.

use color_eyre::eyre::Result;
use tracing::{debug, info, warn};
use trade_control_conventions::{
    BREAK_LABELS, INVALIDATION_LABELS, RETEST_LABELS, SR_LEVEL_LABELS, TRADE_EXPIRY_LABELS,
    matches, prep_name_from_expiry_label,
};

use crate::news_marker::NewsMarker;
use crate::news_window::NewsWindow;
use trading_view::drawings::{Drawing, DrawingStub};
use trading_view::mcp::TvMcp;
// `TimedAnchor::anchor_time` is still used by the single-slot pickers
// (`pick_slot` / nearest-to) below; only the `pair_vertical_lines` pairing
// helper is gone now that windows come from the calendar.
use trading_view::pair_lines::TimedAnchor;

/// Drawing kinds emitted by tv-mcp.
mod kind {
    pub const HORIZONTAL_LINE: &str = "horizontal_line";
    pub const TREND_LINE: &str = "trend_line";
    pub const VERTICAL_LINE: &str = "vertical_line";
    pub const FIB_RETRACEMENT: &str = "fib_retracement";
    /// The polyline / path tool used to mark an M/W reversal. It has no
    /// text property, so it's detected purely by geometry (3 or 4 anchors,
    /// all inside the visible range) — see [`super::classify`].
    pub const PATH: &str = "path";
    /// TradingView short-position tool. Entry is `points[0].price`;
    /// `stopLevel`/`profitLevel` (tick distances) come from properties.
    /// Detected purely by kind — the tool has no label.
    pub const SHORT_POSITION: &str = "short_position";
    /// TradingView long-position tool. Same shape as `SHORT_POSITION`.
    pub const LONG_POSITION: &str = "long_position";
}

/// Direction a position-tool drawing represents, read straight from the
/// drawing kind (`short_position` / `long_position`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionDirection {
    /// `long_position` — SL sits below entry, TP above.
    Long,
    /// `short_position` — SL sits above entry, TP below.
    Short,
}

/// A long/short position tool the operator drew on the chart. The
/// drawing carries an entry anchor (`points[0].price`) and tick-distance
/// `stopLevel`/`profitLevel` in its properties; the conversion to
/// absolute SL/TP prices lives in [`super::position_trade`].
#[derive(Debug, Clone)]
pub struct PositionDrawing {
    /// Long or short, from the drawing kind.
    pub direction: PositionDirection,
    /// The raw drawing — entry is `points[0].price`, levels are in
    /// `properties.stop_level` / `properties.profit_level`.
    pub drawing: Drawing,
}

/// Everything `classify()` extracts from the chart, grouped by role.
#[derive(Debug, Default, Clone)]
pub struct Roles {
    /// Invalidation veto drawing + its raw label (`"too-high"` /
    /// `"too-low"`). Both fields are set together or neither.
    pub invalidation: Option<Drawing>,
    /// Raw invalidation label, kept verbatim so downstream code can
    /// derive both the direction and the basename without reparsing.
    pub invalidation_label: Option<String>,
    /// Trend line for the neckline / break-and-close prep.
    pub break_and_close: Option<Drawing>,
    /// Trend line for the retest prep.
    pub retest: Option<Drawing>,
    /// Fib retracement that anchors the TP geometry.
    pub tp_fib: Option<Drawing>,
    /// Vertical line marking the trade-expiry veto.
    pub trade_expiry: Option<Drawing>,
    /// Blackout (pause/resume) windows, resolved from the calendar at real
    /// event-minute precision. No longer read back off drawn chart lines —
    /// see [`crate::news_window`].
    pub blackout_pairs: Vec<NewsWindow>,
    /// News windows (`news-start`/`news-end`), same calendar-resolved source.
    pub news_pairs: Vec<NewsWindow>,
    /// The individual news events tv-arm reacts to (currency + stars + event
    /// minute), carried alongside the windows purely so tv-arm can draw the
    /// cosmetic markers annotating the *exact* armed set. Never signed, never
    /// gate machinery — cosmetic. Same filter/scope as `news_pairs`, pruned
    /// together in `drop_past_control_pairs`.
    pub news_markers: Vec<NewsMarker>,
    /// Support / resistance horizontal lines. Each one contributes
    /// an `[lo, hi]` price band to the consolidated
    /// `06-close-on-reversal` alert (`inside_window` gets `price`
    /// added; `sr_bands` carries the bands).
    pub sr_levels: Vec<Drawing>,
    /// Prep-expiry cutoff lines: each `<prep>-expiry` vertical line,
    /// resolved to its canonical prep step name (`break-and-close` /
    /// `retest`). One `prep-expire` alert is emitted per entry, bound
    /// to the drawing. Multiple lines for the same step keep only the
    /// latest (older ones are stale leftovers).
    pub prep_expiries: Vec<(String, Drawing)>,
    /// The M/W reversal path: a 3- or 4-anchor `path` drawing wholly
    /// inside the visible range. The path tool has no label, so detection
    /// is geometry-only — anchors are `[A (runup start), B (first point),
    /// C (neckline)]` in draw order, with an optional 4th `D (right
    /// shoulder)` that arms the setup immediately. Latest-wins when several
    /// qualify. `None` for a plain H&S chart.
    pub mw_path: Option<Drawing>,
    /// A long/short **position** tool — the direct-entry path. Detected
    /// by kind alone (the tool has no label). Latest-wins when several
    /// are drawn. `None` unless the operator drew a position tool.
    pub position: Option<PositionDrawing>,
}

/// Anything that can hand us a `Drawing` by ID. The production impl is
/// [`TvMcp`]; tests provide an in-memory map.
pub trait DrawingFetcher {
    /// Fetch the full drawing for `entity_id`.
    fn get_drawing(&self, entity_id: &str) -> Result<Drawing>;
}

impl DrawingFetcher for TvMcp {
    fn get_drawing(&self, entity_id: &str) -> Result<Drawing> {
        TvMcp::get_drawing(self, entity_id)
    }
}

/// Walk `stubs`, fetch each drawing, and group by role. See module
/// docs for the resolution rules.
///
/// `visible_range` is the chart's currently-visible time window (unix
/// seconds). It scopes M/W path detection (a `path` drawing qualifies as the
/// M/W marker only when **all** its anchors fall inside that window) and, when
/// `slot_pref` is [`SlotPref::WindowAware`], drives single-slot role selection
/// too. Pass the window from [`trading_view::mcp::TvMcp::get_range`]
/// (`visible_range`).
///
/// `slot_pref` chooses how single-slot roles (invalidation, neckline, retest,
/// tp_fib, trade_expiry) resolve when several candidates are drawn:
/// [`SlotPref::LatestWins`] for live arming (`--register-plan`) — newest wins —
/// and [`SlotPref::WindowAware`] for an offline / replay build (`--plan-out`
/// alone), which prefers the drawing belonging to the on-screen window so a
/// rewound replay doesn't grab a recent, live-dated drawing.
pub fn classify<F: DrawingFetcher>(
    fetcher: &F,
    stubs: &[DrawingStub],
    visible_range: (i64, i64),
    slot_pref: SlotPref,
) -> Result<Roles> {
    let mut invalidations: Vec<(Drawing, String)> = Vec::new();
    let mut break_lines: Vec<Drawing> = Vec::new();
    let mut tp_fibs: Vec<Drawing> = Vec::new();
    let mut trade_expiries: Vec<Drawing> = Vec::new();
    let mut sr_levels: Vec<Drawing> = Vec::new();
    // Prep-expiry lines, grouped by resolved canonical prep step so a
    // duplicate (re-armed) line keeps only the latest per step.
    let mut prep_expiry_lines: Vec<(&'static str, Drawing)> = Vec::new();
    // Candidate M/W paths: 3-anchor `path` drawings inside the visible
    // range. Latest-wins resolved after the loop.
    let mut mw_paths: Vec<Drawing> = Vec::new();
    // Candidate position tools (long/short). Latest-wins after the loop.
    let mut positions: Vec<PositionDrawing> = Vec::new();

    for stub in stubs {
        let d = fetcher.get_drawing(&stub.id)?;
        let kind = stub.name.as_str();
        // A drawing TradingView read back with a degenerate anchor
        // (`null` price/time — typically a half-drawn or auto-extending
        // channel/fib the operator left lying around) can't be
        // role-matched on its geometry. Skip it with a warning rather
        // than aborting the whole arm — one stray channel should not
        // strand a legitimate setup.
        if d.has_degenerate_point() {
            warn!(
                id = %stub.id,
                kind,
                "drawing skipped — TradingView returned a degenerate anchor (null price/time)",
            );
            continue;
        }
        let lbl_owned = d.label().to_string();
        let lbl = lbl_owned.as_str();

        let role = match kind {
            kind::HORIZONTAL_LINE if matches(lbl, INVALIDATION_LABELS) => {
                invalidations.push((d, lbl.to_lowercase()));
                Some("invalidation")
            }
            kind::HORIZONTAL_LINE if matches(lbl, SR_LEVEL_LABELS) => {
                sr_levels.push(d);
                Some("sr_level")
            }
            kind::TREND_LINE if matches(lbl, BREAK_LABELS) => {
                break_lines.push(d);
                Some("break_and_close")
            }
            // A separately-drawn `retest` trendline is deliberately NOT
            // classified. Historically the retest was its own trendline because
            // a single TradingView drawing could only carry one alert (so the
            // neckline couldn't fire both the break-and-close *and* its own
            // retest). That limitation is gone, and the retest is *by
            // definition* a cross back through the **same neckline** — so the
            // retest role always reuses the resolved neckline
            // (`roles.break_and_close`). Honouring a drawn retest line only let a
            // stale one from an earlier setup silently arm a never-firing cross:
            // USD/ZAR iH&S, 2026-07 — a May-anchored retest line, extrapolated
            // forward with `extend_forward`, sat ~2000 pips below price, so
            // `04-prep-retest` could never cross and the enter never became
            // eligible. Log-and-ignore so such a line is a visible no-op, not a
            // silent footgun.
            kind::TREND_LINE if matches(lbl, RETEST_LABELS) => {
                debug!(
                    id = %stub.id,
                    label = lbl,
                    "drawn `retest` trendline ignored — the neckline serves the retest role",
                );
                None
            }
            kind::FIB_RETRACEMENT => {
                tp_fibs.push(d);
                Some("tp_fib")
            }
            // Prep-expiry lines (`<prep>-expiry`) must be tested before
            // the trade-expiry arm: `trade-expiry` resolves to None here
            // (it's not a prep), so the two never collide, but checking
            // prep-expiry first keeps the intent obvious.
            kind::VERTICAL_LINE if prep_name_from_expiry_label(lbl).is_some() => {
                if let Some(step) = prep_name_from_expiry_label(lbl) {
                    prep_expiry_lines.push((step, d));
                }
                Some("prep_expiry")
            }
            kind::VERTICAL_LINE if matches(lbl, TRADE_EXPIRY_LABELS) => {
                trade_expiries.push(d);
                Some("trade_expiry")
            }
            // Blackout (pause/resume) and news (news-start/news-end) windows are
            // no longer read off drawn chart lines — they come from the calendar
            // planner at real event-minute precision (see `crate::news_window`
            // and the pipeline's `calendar_windows`). A stray pause/news vertical
            // line left on the chart is simply ignored here.
            // M/W reversal path: no label (the tool has no text field),
            // so it's accepted purely on geometry — exactly 3 anchors,
            // all inside the visible range. A path that's the wrong
            // shape or scrolled off-screen is ignored (logged below).
            kind::PATH if is_mw_path(&d, visible_range, slot_pref) => {
                mw_paths.push(d);
                Some("mw_path")
            }
            // Position tools have no label; direction is the kind. We
            // require an entry anchor and both tick levels so a
            // half-drawn tool is ignored rather than guessed at.
            kind::SHORT_POSITION | kind::LONG_POSITION if is_position(&d) => {
                let direction = if kind == kind::SHORT_POSITION {
                    PositionDirection::Short
                } else {
                    PositionDirection::Long
                };
                positions.push(PositionDrawing {
                    direction,
                    drawing: d,
                });
                Some("position")
            }
            _ => None,
        };
        match role {
            Some(r) => debug!(id = %stub.id, kind, label = lbl, role = r, "drawing classified"),
            None => debug!(
                id = %stub.id,
                kind,
                label = lbl,
                "drawing ignored — kind+label combination does not match any role vocabulary",
            ),
        }
    }

    let mut roles = Roles::default();

    // Resolve the neckline first: under `--start` the invalidation picker uses
    // it as a geometric reference (a valid `too-low` floor sits *below* the
    // neckline, a `too-high` cap *above*) to drop a stale line left over from an
    // earlier trade at the wrong price. See [`pick_slot_with_label`].
    let break_and_close = pick_slot(break_lines, "break_and_close", visible_range, slot_pref);
    let neckline_ref = break_and_close
        .as_ref()
        .map(|d| crate::geometry::line_mean_price(&d.prices()))
        .filter(|p| p.is_finite());

    if let Some((d, lbl)) = pick_slot_with_label(
        invalidations,
        "invalidation",
        visible_range,
        slot_pref,
        neckline_ref,
    ) {
        roles.invalidation = Some(d);
        roles.invalidation_label = Some(lbl);
    }
    // Retest = a cross back through the neckline (opposite direction, intrabar).
    // The retest is *by definition* the same neckline as the break-and-close, so
    // it always reuses that resolved drawing. Drawn `retest` trendlines are no
    // longer honoured — they're ignored in `classify` (a stale one could arm a
    // never-firing cross; see the ignore arm there for the USD/ZAR regression).
    roles.retest = break_and_close.clone();
    roles.break_and_close = break_and_close;
    roles.tp_fib = pick_slot(tp_fibs, "tp_fib", visible_range, slot_pref);
    roles.trade_expiry = pick_trade_expiry(trade_expiries, visible_range, slot_pref);

    // Blackout / news windows are populated later, in the pipeline, from the
    // calendar planner (`calendar_windows`) — not from drawn chart lines. So
    // `classify` leaves `roles.blackout_pairs` / `news_pairs` empty here; there
    // is no readback, no independent start/end pruning, and no split-pair abort.
    roles.sr_levels = sr_levels;
    roles.prep_expiries = latest_prep_expiry_per_step(prep_expiry_lines);
    // M/W paths are already in-window-filtered by `is_mw_path`, so latest-wins
    // among qualifiers is correct in both modes (the window filter inside
    // `pick_slot` is a no-op for them). Under `--start` the containment filter
    // is dropped, so select the path whose two shoulders bracket the cursor —
    // and, when an H&S neckline is *also* on the chart, defer to whichever of
    // the two (path neckline vs drawn neckline) is anchored nearer `start`, so a
    // stray M/W path from an earlier setup can't hijack an H&S arm.
    let neckline_anchor = roles.break_and_close.as_ref().map(|d| d.latest_time());
    roles.mw_path = pick_mw_path(mw_paths, slot_pref, neckline_anchor);
    roles.position = latest_position(positions);

    Ok(roles)
}

/// A position tool qualifies only when it has an entry anchor and both
/// tick levels — a half-drawn tool (no stop or no target) is ignored so
/// the pipeline never arms an entry missing a leg.
fn is_position(d: &Drawing) -> bool {
    !d.points.is_empty() && d.properties.stop_level.is_some() && d.properties.profit_level.is_some()
}

/// Latest-wins for position tools (the `PositionDrawing` wrapper means
/// `latest_only` can't be reused directly). Newest anchor time wins; a
/// note is logged when the operator left more than one on the chart.
fn latest_position(mut cands: Vec<PositionDrawing>) -> Option<PositionDrawing> {
    if cands.is_empty() {
        return None;
    }
    if cands.len() > 1 {
        info!(
            count = cands.len(),
            "multiple position tools on chart; picking the latest"
        );
    }
    cands.sort_by_key(|p| p.drawing.latest_time());
    cands.pop()
}

/// A `path` drawing qualifies as the M/W marker iff it has **3 or 4**
/// anchors and every anchor's time falls inside the visible range
/// `[from, to]` (inclusive). Three anchors is the classic
/// `[A runup-start, B left-shoulder, C neckline]`; a 4th anchor is the
/// optional `D right-shoulder` (arms immediately). Off-screen paths are
/// stale leftovers; a path with 2 or 5+ anchors is a fat-fingered shape
/// and is ignored rather than guessed at.
fn is_mw_path(d: &Drawing, (from, to): (i64, i64), pref: SlotPref) -> bool {
    if !matches!(d.points.len(), 3 | 4) {
        return false;
    }
    // `--start` searches the whole chart, so the visible-window containment
    // check is dropped — the bracket-start picker (`pick_mw_path`) selects
    // among all valid-shape paths instead.
    if let SlotPref::NearestTo { .. } = pref {
        return true;
    }
    d.points.iter().all(|p| p.time >= from && p.time <= to)
}

/// Collapse the per-step prep-expiry candidates to one drawing each —
/// the latest line wins (an earlier line on the same step is a stale
/// leftover from a prior arming). Returns `(canonical_step, drawing)`
/// pairs in stable step order (`break-and-close` before `retest`).
fn latest_prep_expiry_per_step(cands: Vec<(&'static str, Drawing)>) -> Vec<(String, Drawing)> {
    use trade_control_conventions::{PREP_BREAK_AND_CLOSE, PREP_RETEST};
    let mut out = Vec::new();
    for step in [PREP_BREAK_AND_CLOSE, PREP_RETEST] {
        let mut for_step: Vec<Drawing> = cands
            .iter()
            .filter(|(s, _)| *s == step)
            .map(|(_, d)| d.clone())
            .collect();
        if for_step.is_empty() {
            continue;
        }
        if for_step.len() > 1 {
            info!(
                count = for_step.len(),
                step, "multiple prep-expiry lines for step; picking the latest"
            );
        }
        for_step.sort_by_key(|d| d.latest_time());
        if let Some(d) = for_step.pop() {
            out.push((step.to_string(), d));
        }
    }
    out
}

/// How a single-slot role (invalidation, neckline, retest, tp_fib,
/// trade_expiry) breaks ties **after** the visible-window filter (see
/// [`pick_slot`]) has dropped off-screen drawings. The window filter runs in
/// both modes; `SlotPref` only governs the choice among the in-window
/// survivors.
///
/// The two modes mirror the two ways `tv-arm` is run (see [`classify`]):
///
/// - [`SlotPref::LatestWins`] — live arming (`--register-plan`). Among the
///   in-window drawings the operator just (re)drew, the newest is
///   authoritative; an older one is a stale leftover. This is the
///   long-standing behaviour (now scoped to the visible window first).
/// - [`SlotPref::WindowAware`] — offline / replay build (`--plan-out`
///   without `--register-plan`). The chart is rewound to a historical setup;
///   among the in-window survivors prefer the one fully inside the window,
///   then the one anchored just before the window start — see
///   [`pick_window_aware`]. (When the window filter leaves *nothing* — a
///   chart rewound to the left of every drawing — `pick_slot` falls back to
///   the full set so this before-and-closest logic still has candidates.)
#[derive(Debug, Clone, Copy)]
pub enum SlotPref {
    /// Newest anchor time wins (live arming).
    LatestWins,
    /// Prefer the drawing belonging to the visible window `(from, to)`
    /// (replay build).
    WindowAware((i64, i64)),
    /// `--start`: ignore the visible window entirely and select each role by
    /// its **nearest-to-start** drawing, walking in the role's natural
    /// direction (see the per-role pickers). `start` is a unix second. The
    /// default tiebreak for the generic single-slot roles (neckline / retest /
    /// tp_fib) is *nearest anchored at-or-before start*; `invalidation`,
    /// `trade_expiry`, and the M/W path apply their own directional rules.
    NearestTo { start: i64 },
}

/// Pick the M/W path drawing.
///
/// In the window-scoped modes (`LatestWins` / `WindowAware`) the paths were
/// already visible-window-filtered by [`is_mw_path`], so latest-wins among the
/// qualifiers is correct. Under `--start` ([`SlotPref::NearestTo`]) that filter
/// is dropped, so select the path whose two **shoulders bracket the cursor**:
/// `B left-shoulder <= start <= D right-shoulder`. When the right shoulder
/// isn't drawn (3-anchor path) it's still forming, so the rule relaxes to
/// `start >= B`. Among the bracketing paths the latest wins; if none bracket
/// `start` (unusual — a chart with only unrelated paths) fall back to plain
/// latest so a single stray path isn't silently dropped.
///
/// **H&S neckline tiebreak (`--start` only).** A chart can carry both an M/W
/// path *and* an H&S neckline — e.g. a stray path from an earlier setup left on
/// the chart while the operator journals an H&S. Because a path's presence alone
/// routes the whole arm to M/W ([`super::pipeline`] keys on `roles.mw_path`), an
/// old path must not win when the drawn H&S neckline is the setup actually being
/// journaled. So when `neckline_anchor` is `Some`, compare the chosen path's own
/// neckline anchor `C (points[2])` to it: whichever is anchored **nearer
/// `start`** wins. If the drawn neckline is nearer, the path is dropped
/// (`None` → H&S arm). This is what makes the 3-anchor relax rule safe — that
/// rule (`start >= B`) otherwise matches *any* old path whose left shoulder
/// predates the cursor.
///
/// Anchor order (see [`is_mw_path`]): `points[0]=A run-up-start`,
/// `points[1]=B left-shoulder`, `points[2]=C neckline`, `points[3]=D
/// right-shoulder` (optional).
fn pick_mw_path(
    cands: Vec<Drawing>,
    pref: SlotPref,
    neckline_anchor: Option<i64>,
) -> Option<Drawing> {
    let SlotPref::NearestTo { start } = pref else {
        return pick_slot(cands, "mw_path", (i64::MIN, i64::MAX), SlotPref::LatestWins);
    };
    let brackets = |d: &Drawing| {
        let Some(b) = d.points.get(1).map(|p| p.time) else {
            return false;
        };
        match d.points.get(3).map(|p| p.time) {
            Some(right) => b <= start && start <= right,
            None => start >= b,
        }
    };
    let chosen = cands
        .iter()
        .filter(|d| brackets(d))
        .max_by_key(|d| d.latest_time())
        .cloned()
        .or_else(|| {
            debug!(
                role = "mw_path",
                start, "no M/W path whose shoulders bracket --start; falling back to latest"
            );
            cands.into_iter().max_by_key(|d| d.latest_time())
        })?;

    // With both a path and a drawn H&S neckline on the chart, defer to whichever
    // is anchored nearer the cursor. The path's own neckline is anchor C
    // (`points[2]`); fall back to its latest anchor if the shape is short.
    if let Some(neck) = neckline_anchor {
        let path_neck = chosen
            .points
            .get(2)
            .map(|p| p.time)
            .unwrap_or_else(|| chosen.latest_time());
        if (neck - start).abs() < (path_neck - start).abs() {
            debug!(
                role = "mw_path",
                start,
                path_neck,
                neckline_anchor = neck,
                "drawn H&S neckline is nearer --start than the M/W path; dropping the path (H&S arm)"
            );
            return None;
        }
    }
    Some(chosen)
}

/// Pick a single-slot drawing, window-filtering first then breaking ties
/// under `pref`.
///
/// **Window filter first (both modes).** Drawings whose time-span lies
/// *entirely* outside the visible window `[from, to]` are stale leftovers from
/// another part of the timeline — they're dropped before any tiebreak so a
/// recent off-screen drawing can never out-rank the in-view one. This is the
/// fix for the recurring "armed against the wrong pattern" bug: the window
/// applies to **live arming too**, not just replay. Intersection (not
/// containment) keeps a line that spans the whole view or pokes past one edge.
///
/// After the filter:
/// - ≥1 in-window candidate → tiebreak among those (`>1` logged at WARN as
///   genuine on-screen ambiguity the operator should clean up).
/// - none in-window → fall back to the full candidate set so we never select
///   *nothing* (this preserves the replay before-and-closest behaviour for a
///   chart rewound to the left of every drawing).
///
/// The per-mode tiebreak itself: `LatestWins` keeps the newest; `WindowAware`
/// defers to [`pick_window_aware`].
fn pick_slot(
    cands: Vec<Drawing>,
    role: &str,
    (from, to): (i64, i64),
    pref: SlotPref,
) -> Option<Drawing> {
    if cands.is_empty() {
        return None;
    }
    // `--start` ignores the visible window: search the whole chart and let the
    // nearest-to-start tiebreak choose. Skip the window partition entirely.
    if let SlotPref::NearestTo { .. } = pref {
        return finish_slot_pick(cands, role, pref);
    }
    let total = cands.len();
    let (in_window, out): (Vec<Drawing>, Vec<Drawing>) = cands
        .into_iter()
        .partition(|d| d.intersects_window(from, to));
    if !out.is_empty() {
        info!(
            role,
            in_window = in_window.len(),
            dropped_out_of_window = out.len(),
            "filtered drawings to the visible window before role-matching"
        );
    }
    // Fall back to the full set only when the window left nothing.
    let (survivors, scoped_to_window) = if in_window.is_empty() {
        debug!(
            role,
            total, "no in-window drawing; using full candidate set"
        );
        (out, false)
    } else {
        (in_window, true)
    };
    if scoped_to_window && survivors.len() > 1 {
        warn!(
            count = survivors.len(),
            role, "multiple in-window drawings for role; picking by preference (ambiguous chart)"
        );
    }
    finish_slot_pick(survivors, role, pref)
}

/// Run the per-mode tiebreak over `cands` (already window-filtered by
/// [`pick_slot`]). Split out so the window filter and the tiebreak read as
/// two distinct steps.
fn finish_slot_pick(mut cands: Vec<Drawing>, role: &str, pref: SlotPref) -> Option<Drawing> {
    match pref {
        SlotPref::LatestWins => {
            cands.sort_by_key(|d| d.latest_time());
            cands.pop()
        }
        SlotPref::WindowAware(view) => pick_window_aware(cands, role, view),
        SlotPref::NearestTo { start } => pick_nearest_before(cands, role, start),
    }
}

/// `--start` tiebreak for the generic single-slot roles (neckline / retest /
/// tp_fib). These are drawn *before* the setup completes, so the right one is
/// the drawing anchored **at-or-before `start`, closest to it** (largest
/// `latest_time()` that is `<= start`). If none is at-or-before start — e.g. a
/// chart where every candidate is to the right — fall back to the one whose
/// anchor is nearest to `start` in absolute terms, so we never select nothing.
fn pick_nearest_before(cands: Vec<Drawing>, role: &str, start: i64) -> Option<Drawing> {
    let before = cands
        .iter()
        .filter(|d| d.latest_time() <= start)
        .max_by_key(|d| d.latest_time())
        .cloned();
    if let Some(d) = before {
        return Some(d);
    }
    debug!(
        role,
        start, "no drawing anchored at-or-before --start; using absolute-nearest anchor"
    );
    cands
        .into_iter()
        .min_by_key(|d| (d.latest_time() - start).abs())
}

/// Pick the trade-expiry vertical line.
///
/// Unlike the other single-slot roles, the trade-expiry marker is *meant* to
/// sit at or just past the visible window's right edge (it bounds a trade that
/// may run days past what's on screen). So the generic [`pick_slot`] window
/// filter — which drops anything entirely right of `to` — would throw away a
/// perfectly valid expiry. Instead:
///
/// - **In-window** expiries (intersect `[from, to]`) qualify, as do ones
///   **within a forward margin** of `to` (the margin is the window's own width,
///   so it scales with timeframe; for `LatestWins`'s unbounded window every
///   expiry qualifies).
/// - Among the qualifiers, prefer the one **nearest the right edge** — the
///   expiry the operator drew for *this* setup, not a stale one far to the
///   right or an old one to the left.
/// - If nothing qualifies, fall back to the full set under the mode tiebreak so
///   we never silently drop the only expiry on the chart.
fn pick_trade_expiry(
    cands: Vec<Drawing>,
    (from, to): (i64, i64),
    pref: SlotPref,
) -> Option<Drawing> {
    const ROLE: &str = "trade_expiry";
    if cands.is_empty() {
        return None;
    }
    // `--start`: the expiry sits *forward* of the setup, so pick the nearest
    // vertical at-or-after `start`; if none is forward (a chart with only a
    // past expiry) fall back to the absolute-nearest so we never drop it.
    if let SlotPref::NearestTo { start } = pref {
        let after = cands
            .iter()
            .filter(|d| d.anchor_time() >= start)
            .min_by_key(|d| d.anchor_time())
            .cloned();
        return after.or_else(|| {
            debug!(
                role = ROLE,
                start, "no trade-expiry at-or-after --start; using absolute-nearest anchor"
            );
            cands
                .into_iter()
                .min_by_key(|d| (d.anchor_time() - start).abs())
        });
    }
    // Forward margin = window width (saturating), so it scales with timeframe.
    let width = to.saturating_sub(from);
    let margin_to = to.saturating_add(width);
    let total = cands.len();
    let (qualifying, out): (Vec<Drawing>, Vec<Drawing>) = cands.into_iter().partition(|d| {
        d.intersects_window(from, to) || (d.earliest_time() > to && d.earliest_time() <= margin_to)
    });
    if !out.is_empty() {
        info!(
            role = ROLE,
            in_window = qualifying.len(),
            dropped_out_of_window = out.len(),
            "filtered drawings to the visible window before role-matching"
        );
    }
    let pool = if qualifying.is_empty() {
        debug!(
            role = ROLE,
            total, "no in-window/near expiry; using full candidate set"
        );
        return finish_slot_pick(out, ROLE, pref);
    } else {
        qualifying
    };
    if pool.len() > 1 {
        warn!(
            count = pool.len(),
            role = ROLE,
            "multiple in-window trade-expiry lines; picking the one nearest the right edge"
        );
    }
    // Nearest the right edge = largest anchor time (closest to / just past `to`).
    pool.into_iter().max_by_key(|d| d.anchor_time())
}

/// Window-aware single-slot pick for replay builds. In order of preference:
///
/// 1. A drawing whose anchors all sit **inside** the visible range
///    `[from, to]` — among those, the latest wins.
/// 2. Else the drawing anchored **before and closest** to the window start
///    (largest `latest_time()` that is `<= from`) — a neckline drawn just
///    left of the replay cursor still works.
/// 3. Else plain latest-wins, so we never select *nothing* and silently
///    regress a chart whose drawings are all to the right of the window.
fn pick_window_aware(cands: Vec<Drawing>, role: &str, (from, to): (i64, i64)) -> Option<Drawing> {
    let in_window = cands
        .iter()
        .filter(|d| !d.points.is_empty() && d.points.iter().all(|p| p.time >= from && p.time <= to))
        .max_by_key(|d| d.latest_time())
        .cloned();
    if let Some(d) = in_window {
        return Some(d);
    }
    debug!(
        role,
        "no in-window drawing; falling back to before-and-closest"
    );
    let before = cands
        .iter()
        .filter(|d| d.latest_time() <= from)
        .max_by_key(|d| d.latest_time())
        .cloned();
    if let Some(d) = before {
        return Some(d);
    }
    debug!(
        role,
        "no drawing at-or-before window start; falling back to latest"
    );
    cands.into_iter().max_by_key(|d| d.latest_time())
}

/// Variant of [`pick_slot`] for drawings that carry an associated label
/// (currently just `invalidation`). Selects the drawing under `pref`, then
/// returns it with its label.
///
/// `neckline_ref` is the resolved neckline's mean price (or `None` if no
/// neckline is on the chart). Under `--start` it drives a **side-of-neckline
/// filter** that runs *before* the nearest-to-start tiebreak: a valid `too-low`
/// floor sits **below** the neckline and a valid `too-high` cap **above** it, so
/// any candidate on the wrong side is a stale line left over from a different
/// trade (e.g. an old `too-low` still drawn up near a prior head) and is
/// dropped. This is what stops the picker grabbing a stale invalidation purely
/// because its anchor *time* happened to sit nearer the cursor than the real
/// one's (AUD/JPY IH&S 2026-06-29: a stale `too-low` at 112.993 out-timed the
/// real floor at 111.288 and blocked every entry via the baked entry-level
/// veto). If the filter would drop everything (no neckline, or all candidates on
/// the wrong side) it's skipped so we never select nothing.
fn pick_slot_with_label(
    cands: Vec<(Drawing, String)>,
    role: &str,
    window: (i64, i64),
    pref: SlotPref,
    neckline_ref: Option<f64>,
) -> Option<(Drawing, String)> {
    if cands.is_empty() {
        return None;
    }
    // Side-of-neckline filter (— `--start` only). Keep each candidate only if
    // its price is on the geometrically-correct side of the neckline for its own
    // label. Applied before splitting labels off, since the side depends on the
    // per-candidate label. Skipped when it would empty the set.
    let cands = filter_invalidation_by_neckline_side(cands, pref, neckline_ref, role);
    // Split labels off into a lookup, pick on the drawings alone (so the
    // window-filter + tiebreak logic is shared), then re-attach the chosen
    // label by id.
    let labels: std::collections::HashMap<String, String> = cands
        .iter()
        .map(|(d, lbl)| (d.id.clone(), lbl.clone()))
        .collect();
    let drawings: Vec<Drawing> = cands.into_iter().map(|(d, _)| d).collect();
    // The invalidation horizontals (`too-low` / `too-high`) *bracket* the
    // pattern, so under `--start` the right one is the nearest **either side**
    // of `start`, not nearest-before (a `too-high` cap above the right shoulder
    // may be anchored just after the cursor). Every other single-slot role
    // routes through `pick_slot`'s nearest-before default.
    //
    // Secondary tiebreak on **level**: when two invalidation lines are anchored
    // at the same time (an ambiguous chart with both a `too-low` and a
    // `too-high` drawn — e.g. two overlapping H&S setups), the time-distance
    // tiebreak is a dead tie and `min_by_key` would pick arbitrarily by vec
    // order, silently choosing the trade direction. Break such ties by picking
    // the line **closest to the neckline**: a genuine cap/floor hugs the
    // neckline, whereas a line far from it is a stale leftover from a larger,
    // different setup. (Stellar iH&S/H&S 2026-07-06: too-low 143.1 and too-high
    // 202.1 tied on time; neckline 200.2 → too-high wins → short, the intended
    // trade.) Only applies on an exact time tie; when times differ, time still
    // dominates.
    let chosen = if let SlotPref::NearestTo { start } = pref {
        let level_gap = |d: &Drawing| -> f64 {
            match neckline_ref {
                Some(neck) => (crate::geometry::horizontal_price(&d.prices()) - neck).abs(),
                None => f64::INFINITY,
            }
        };
        drawings.into_iter().min_by(|a, b| {
            let ta = (a.anchor_time() - start).abs();
            let tb = (b.anchor_time() - start).abs();
            ta.cmp(&tb).then_with(|| {
                level_gap(a)
                    .partial_cmp(&level_gap(b))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        })?
    } else {
        pick_slot(drawings, role, window, pref)?
    };
    let lbl = labels.get(&chosen.id).cloned().unwrap_or_default();
    Some((chosen, lbl))
}

/// Drop invalidation candidates on the geometrically-wrong side of the neckline
/// (`--start` only). A `too-low` floor must sit **below** the neckline, a
/// `too-high` cap **above** it. Returns the filtered set, or the original set
/// unchanged when the filter doesn't apply (not `--start`, no neckline, a
/// non-finite candidate price) or would drop everything (so we never select
/// nothing). Each dropped line is logged at debug.
fn filter_invalidation_by_neckline_side(
    cands: Vec<(Drawing, String)>,
    pref: SlotPref,
    neckline_ref: Option<f64>,
    role: &str,
) -> Vec<(Drawing, String)> {
    let (SlotPref::NearestTo { .. }, Some(neck)) = (pref, neckline_ref) else {
        return cands;
    };
    let correct_side = |d: &Drawing, lbl: &str| -> bool {
        let price = crate::geometry::horizontal_price(&d.prices());
        if !price.is_finite() {
            return true; // can't judge → keep (never drop on missing data)
        }
        let l = lbl.trim().to_ascii_lowercase();
        match l.as_str() {
            "too-low" => price < neck,  // long floor sits below the neckline
            "too-high" => price > neck, // short cap sits above the neckline
            _ => true,                  // unknown label → keep
        }
    };
    let (kept, dropped): (Vec<_>, Vec<_>) =
        cands.into_iter().partition(|(d, lbl)| correct_side(d, lbl));
    if kept.is_empty() {
        // Everything is on the "wrong" side — the neckline reference is
        // probably itself off (or the operator drew an unusual chart). Don't
        // strand the setup; fall back to the full set.
        debug!(
            role,
            neckline_ref = neck,
            "side-of-neckline filter would drop every invalidation; keeping all"
        );
        return dropped;
    }
    for (d, lbl) in &dropped {
        debug!(
            role,
            id = %d.id,
            label = %lbl,
            price = crate::geometry::horizontal_price(&d.prices()),
            neckline_ref = neck,
            "invalidation dropped — wrong side of the neckline (stale line?)"
        );
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use trading_view::drawings::{Point, Properties};

    /// A visible range wide enough to contain every H&S fixture anchor —
    /// M/W path detection is the only thing that consults it, and the
    /// H&S tests carry no paths, so any all-encompassing window works.
    const ANY_RANGE: (i64, i64) = (0, i64::MAX);

    /// In-memory fetcher backed by a HashMap<id, Drawing>.
    struct MockFetcher {
        drawings: HashMap<String, Drawing>,
    }

    impl DrawingFetcher for MockFetcher {
        fn get_drawing(&self, entity_id: &str) -> Result<Drawing> {
            self.drawings
                .get(entity_id)
                .cloned()
                .ok_or_else(|| color_eyre::eyre::eyre!("unknown id {entity_id}"))
        }
    }

    fn drawing(id: &str, label: &str, points: Vec<(i64, f64)>) -> Drawing {
        Drawing {
            id: id.to_string(),
            points: points
                .into_iter()
                .map(|(t, p)| Point { time: t, price: p })
                .collect(),
            properties: Properties {
                text: Some(label.to_string()),
                ..Default::default()
            },
        }
    }

    /// A position-tool drawing: single entry anchor + tick levels, no
    /// label (matches what tv-mcp emits for long/short position tools).
    fn position(id: &str, entry: f64, t: i64, stop_level: f64, profit_level: f64) -> Drawing {
        Drawing {
            id: id.to_string(),
            points: vec![Point {
                time: t,
                price: entry,
            }],
            properties: Properties {
                text: None,
                stop_level: Some(stop_level),
                profit_level: Some(profit_level),
                qty: Some(0.01),
            },
        }
    }

    fn stub(id: &str, name: &str) -> DrawingStub {
        DrawingStub {
            id: id.to_string(),
            name: name.to_string(),
        }
    }

    fn fixture(items: Vec<(DrawingStub, Drawing)>) -> (Vec<DrawingStub>, MockFetcher) {
        let mut stubs = Vec::new();
        let mut map = HashMap::new();
        for (s, d) in items {
            map.insert(s.id.clone(), d);
            stubs.push(s);
        }
        (stubs, MockFetcher { drawings: map })
    }

    #[test]
    fn classifies_full_short_h_and_s_chart() {
        // Realistic short-trade chart: invalidation cap, neckline,
        // retest, fib, trade-expiry, and one S/R level. Stray
        // blackout/news vertical lines are left on the chart to prove
        // `classify` now IGNORES them (windows come from the calendar).
        let (stubs, mcp) = fixture(vec![
            (
                stub("inv", "horizontal_line"),
                drawing("inv", "too-high", vec![(100, 1.25)]),
            ),
            (
                stub("neck", "trend_line"),
                drawing("neck", "neckline", vec![(50, 1.10), (200, 1.10)]),
            ),
            (
                stub("re", "trend_line"),
                drawing("re", "retest", vec![(50, 1.10), (200, 1.10)]),
            ),
            (
                stub("fib", "fib_retracement"),
                drawing("fib", "", vec![(50, 1.20), (200, 1.10)]),
            ),
            (
                stub("exp", "vertical_line"),
                drawing("exp", "trade-expiry", vec![(500, 1.0)]),
            ),
            (
                stub("bs", "vertical_line"),
                drawing("bs", "blackout-start", vec![(300, 1.0)]),
            ),
            (
                stub("be", "vertical_line"),
                drawing("be", "blackout-end", vec![(350, 1.0)]),
            ),
            (
                stub("ns", "vertical_line"),
                drawing("ns", "news-start", vec![(400, 1.0)]),
            ),
            (
                stub("ne", "vertical_line"),
                drawing("ne", "news-end", vec![(450, 1.0)]),
            ),
            (
                stub("sr", "horizontal_line"),
                drawing("sr", "support", vec![(100, 1.05)]),
            ),
        ]);

        let roles = classify(&mcp, &stubs, ANY_RANGE, SlotPref::LatestWins).expect("classify ok");

        assert!(roles.invalidation.is_some());
        assert_eq!(roles.invalidation_label.as_deref(), Some("too-high"));
        assert_eq!(roles.break_and_close.as_ref().unwrap().id, "neck");
        // The drawn `retest` line ("re") is no longer classified; the retest
        // role reuses the resolved neckline ("neck").
        assert_eq!(roles.retest.as_ref().unwrap().id, "neck");
        assert_eq!(roles.tp_fib.as_ref().unwrap().id, "fib");
        assert_eq!(roles.trade_expiry.as_ref().unwrap().id, "exp");

        // Drawn pause/resume and news-start/news-end lines are no longer
        // classified — the calendar is the sole source of these windows.
        assert!(roles.blackout_pairs.is_empty());
        assert!(roles.news_pairs.is_empty());

        assert_eq!(roles.sr_levels.len(), 1);
        assert_eq!(roles.sr_levels[0].id, "sr");
    }

    #[test]
    fn neckline_serves_the_retest_when_no_retest_line_is_drawn() {
        // No separate `retest` trendline: the retest role reuses the neckline
        // drawing so `04-prep-retest` crosses the identical geometry.
        let (stubs, mcp) = fixture(vec![(
            stub("neck", "trend_line"),
            drawing("neck", "neckline", vec![(50, 1.10), (200, 1.10)]),
        )]);

        let roles = classify(&mcp, &stubs, ANY_RANGE, SlotPref::LatestWins).expect("classify ok");
        assert_eq!(roles.break_and_close.as_ref().unwrap().id, "neck");
        assert_eq!(
            roles.retest.as_ref().unwrap().id,
            "neck",
            "the retest role falls back to the neckline drawing"
        );
    }

    #[test]
    fn a_drawn_retest_line_is_ignored_neckline_wins() {
        // Support for separately-drawn `retest` trendlines was dropped: the
        // retest is by definition the same neckline, and a stale drawn retest
        // line (anchored weeks off) could silently arm a cross that never fires
        // (USD/ZAR iH&S 2026-07). The drawn `retest` line is now a no-op; the
        // retest role always reuses the resolved neckline.
        let (stubs, mcp) = fixture(vec![
            (
                stub("neck", "trend_line"),
                drawing("neck", "neckline", vec![(50, 1.10), (200, 1.10)]),
            ),
            (
                stub("re", "trend_line"),
                drawing("re", "retest", vec![(60, 1.11), (210, 1.11)]),
            ),
        ]);

        let roles = classify(&mcp, &stubs, ANY_RANGE, SlotPref::LatestWins).expect("classify ok");
        assert_eq!(roles.break_and_close.as_ref().unwrap().id, "neck");
        assert_eq!(
            roles.retest.as_ref().unwrap().id,
            "neck",
            "a drawn retest line is ignored; the neckline serves the retest role"
        );
    }

    #[test]
    fn no_neckline_and_no_retest_leaves_the_retest_role_empty() {
        // With neither drawing there's nothing to derive the retest from.
        let (stubs, mcp) = fixture(vec![(
            stub("inv", "horizontal_line"),
            drawing("inv", "too-high", vec![(100, 1.25)]),
        )]);

        let roles = classify(&mcp, &stubs, ANY_RANGE, SlotPref::LatestWins).expect("classify ok");
        assert!(roles.break_and_close.is_none());
        assert!(
            roles.retest.is_none(),
            "no neckline to fall back to → no retest"
        );
    }

    #[test]
    fn stale_retest_line_before_start_does_not_win_over_neckline() {
        // USD/ZAR iH&S regression (2026-07): a `retest` trendline left over
        // from an earlier setup was anchored ~weeks before `--start` and, with
        // its own slope extrapolated forward, sat far from the neckline — so
        // `04-prep-retest` armed a cross that could never fire. Under `--start`
        // the old drawn-retest picker took the stale line; now the drawn line
        // is ignored and the retest reuses the in-window neckline instead.
        let start = 500;
        let (stubs, mcp) = fixture(vec![
            // In-window neckline the operator is actually journaling.
            (
                stub("neck", "trend_line"),
                drawing("neck", "neckline", vec![(400, 16.28), (480, 16.26)]),
            ),
            // Stale retest line anchored long before start, far in price.
            (
                stub("stale", "trend_line"),
                drawing("stale", "retest", vec![(50, 16.36), (80, 16.35)]),
            ),
        ]);

        let roles =
            classify(&mcp, &stubs, ANY_RANGE, SlotPref::NearestTo { start }).expect("classify ok");
        assert_eq!(roles.break_and_close.as_ref().unwrap().id, "neck");
        assert_eq!(
            roles.retest.as_ref().unwrap().id,
            "neck",
            "the stale drawn retest line must not win; retest reuses the neckline",
        );
    }

    #[test]
    fn degenerate_drawing_is_skipped_not_fatal() {
        // A stray `parallel_channel` whose third anchor read back with a
        // null price (NaN sentinel) must NOT abort classification — it is
        // skipped, and the legitimate neckline beside it still resolves.
        let mut channel = drawing("chan", "", vec![(50, 1.10), (200, 1.10)]);
        channel.points.push(Point {
            time: 1772582400,
            price: f64::NAN,
        });
        let (stubs, mcp) = fixture(vec![
            (stub("chan", "parallel_channel"), channel),
            (
                stub("neck", "trend_line"),
                drawing("neck", "neckline", vec![(50, 1.10), (200, 1.10)]),
            ),
        ]);

        let roles = classify(&mcp, &stubs, ANY_RANGE, SlotPref::LatestWins)
            .expect("a degenerate channel must not abort the arm");
        assert_eq!(
            roles.break_and_close.as_ref().unwrap().id,
            "neck",
            "the neckline still resolves alongside the skipped channel"
        );
    }

    #[test]
    fn drawn_pause_and_news_lines_are_ignored_regardless_of_window() {
        // Blackout/news windows are resolved from the calendar, not from drawn
        // chart lines. Any pause/resume/news-start/news-end verticals left on
        // the chart — in or out of the visible window — are simply ignored, so
        // `classify` never populates the pair lists. (This replaces the old
        // window-filter tests, whose independent start/end pruning caused the
        // `--start` straddle "1 start / 2 ends" abort.)
        let (stubs, mcp) = fixture(vec![
            (
                stub("bs", "vertical_line"),
                drawing("bs", "pause", vec![(300, 1.0)]),
            ),
            (
                stub("be", "vertical_line"),
                drawing("be", "resume", vec![(350, 1.0)]),
            ),
            (
                stub("ns", "vertical_line"),
                drawing("ns", "news-start", vec![(400, 1.0)]),
            ),
            (
                stub("ne", "vertical_line"),
                drawing("ne", "news-end", vec![(450, 1.0)]),
            ),
        ]);

        let roles = classify(&mcp, &stubs, (200, 500), SlotPref::LatestWins).expect("classify ok");
        assert!(roles.blackout_pairs.is_empty());
        assert!(roles.news_pairs.is_empty());
    }

    #[test]
    fn empty_chart_yields_empty_roles() {
        let (stubs, mcp) = fixture(vec![]);
        let roles = classify(&mcp, &stubs, ANY_RANGE, SlotPref::LatestWins).expect("classify ok");
        assert!(roles.invalidation.is_none());
        assert!(roles.break_and_close.is_none());
        assert!(roles.blackout_pairs.is_empty());
        assert!(roles.news_pairs.is_empty());
        assert!(roles.sr_levels.is_empty());
    }

    #[test]
    fn duplicate_role_keeps_latest() {
        // Two necklines: old at t=100, new at t=500. Latest wins.
        let (stubs, mcp) = fixture(vec![
            (
                stub("old", "trend_line"),
                drawing("old", "neckline", vec![(50, 1.0), (100, 1.0)]),
            ),
            (
                stub("new", "trend_line"),
                drawing("new", "neckline", vec![(400, 1.0), (500, 1.0)]),
            ),
        ]);
        let roles = classify(&mcp, &stubs, ANY_RANGE, SlotPref::LatestWins).expect("ok");
        assert_eq!(roles.break_and_close.unwrap().id, "new");
    }

    #[test]
    fn duplicate_invalidation_keeps_latest_with_label() {
        let (stubs, mcp) = fixture(vec![
            (
                stub("old", "horizontal_line"),
                drawing("old", "too-low", vec![(100, 1.0)]),
            ),
            (
                stub("new", "horizontal_line"),
                drawing("new", "too-high", vec![(500, 1.5)]),
            ),
        ]);
        let roles = classify(&mcp, &stubs, ANY_RANGE, SlotPref::LatestWins).expect("ok");
        assert_eq!(roles.invalidation.as_ref().unwrap().id, "new");
        assert_eq!(roles.invalidation_label.as_deref(), Some("too-high"));
    }

    #[test]
    fn window_aware_prefers_in_window_neckline_over_newer_out_of_window() {
        // The replay bug: an old in-window neckline (t=100..200) and a recent
        // out-of-window one (t=900..1000). Latest-wins would grab the recent
        // one (baking an anchor outside the replayed window); window-aware must
        // pick the in-window neckline.
        let (stubs, mcp) = fixture(vec![
            (
                stub("hist", "trend_line"),
                drawing("hist", "neckline", vec![(100, 1.0), (200, 1.0)]),
            ),
            (
                stub("recent", "trend_line"),
                drawing("recent", "neckline", vec![(900, 1.5), (1000, 1.5)]),
            ),
        ]);
        let view = (50, 300);
        let roles = classify(&mcp, &stubs, view, SlotPref::WindowAware(view)).expect("ok");
        assert_eq!(roles.break_and_close.unwrap().id, "hist");
    }

    #[test]
    fn window_aware_falls_back_to_before_and_closest_when_none_in_window() {
        // No neckline fully inside the window [500, 800]. Two sit entirely
        // before it (t≤500): the one anchored closest to the window start
        // (t=450) wins over the older one (t=200). A neckline to the right of
        // the window (t=900+) is ignored.
        let (stubs, mcp) = fixture(vec![
            (
                stub("far", "trend_line"),
                drawing("far", "neckline", vec![(100, 1.0), (200, 1.0)]),
            ),
            (
                stub("near", "trend_line"),
                drawing("near", "neckline", vec![(400, 1.0), (450, 1.0)]),
            ),
            (
                stub("future", "trend_line"),
                drawing("future", "neckline", vec![(900, 1.0), (1000, 1.0)]),
            ),
        ]);
        let view = (500, 800);
        let roles = classify(&mcp, &stubs, view, SlotPref::WindowAware(view)).expect("ok");
        assert_eq!(roles.break_and_close.unwrap().id, "near");
    }

    #[test]
    fn window_aware_last_resort_is_latest_when_all_to_the_right() {
        // Every neckline sits to the right of the window [0, 100] — none is
        // in-window and none is at-or-before the start. Last-resort latest-wins
        // so we never select nothing.
        let (stubs, mcp) = fixture(vec![
            (
                stub("a", "trend_line"),
                drawing("a", "neckline", vec![(200, 1.0), (300, 1.0)]),
            ),
            (
                stub("b", "trend_line"),
                drawing("b", "neckline", vec![(600, 1.0), (700, 1.0)]),
            ),
        ]);
        let view = (0, 100);
        let roles = classify(&mcp, &stubs, view, SlotPref::WindowAware(view)).expect("ok");
        assert_eq!(roles.break_and_close.unwrap().id, "b");
    }

    // ===== `--start` (SlotPref::NearestTo) whole-chart matching ==========

    /// `--start` picks the neckline anchored **before and nearest** the cursor,
    /// ignoring the visible window entirely — a later (future) neckline that a
    /// naive latest-wins would grab is skipped. This is the journaling case: the
    /// chart shows the whole trade + future candles, but arming is anchored to
    /// the shoulder moment.
    #[test]
    fn nearest_to_picks_neckline_before_and_nearest_start() {
        let (stubs, mcp) = fixture(vec![
            (
                stub("old", "trend_line"),
                drawing("old", "neckline", vec![(100, 1.0), (200, 1.0)]),
            ),
            (
                stub("near", "trend_line"),
                drawing("near", "neckline", vec![(400, 1.0), (500, 1.0)]),
            ),
            (
                stub("future", "trend_line"),
                drawing("future", "neckline", vec![(900, 1.0), (1000, 1.0)]),
            ),
        ]);
        // start=600: `near` (latest_time 500 ≤ 600) is nearest-before; `future`
        // (900) is after start and must not win despite being newest.
        let roles = classify(&mcp, &stubs, ANY_RANGE, SlotPref::NearestTo { start: 600 })
            .expect("classify ok");
        assert_eq!(roles.break_and_close.unwrap().id, "near");
    }

    /// The invalidation horizontals bracket the pattern, so `--start` takes the
    /// nearest **either side** of the cursor — a `too-high` cap anchored just
    /// *after* start still wins if it's closest.
    #[test]
    fn nearest_to_invalidation_takes_nearest_either_side() {
        let (stubs, mcp) = fixture(vec![
            (
                stub("far-before", "horizontal_line"),
                drawing("far-before", "too-low", vec![(100, 1.0)]),
            ),
            (
                stub("just-after", "horizontal_line"),
                drawing("just-after", "too-high", vec![(650, 1.5)]),
            ),
        ]);
        // start=600: `just-after` (|650-600|=50) beats `far-before`
        // (|100-600|=500), even though it's anchored after the cursor.
        let roles = classify(&mcp, &stubs, ANY_RANGE, SlotPref::NearestTo { start: 600 })
            .expect("classify ok");
        assert_eq!(roles.invalidation.as_ref().unwrap().id, "just-after");
        assert_eq!(roles.invalidation_label.as_deref(), Some("too-high"));
    }

    /// THE BUG (AUD/JPY IH&S 2026-06-29): two `too-low` lines on the chart — the
    /// real floor at 111.288 (below the neckline ~111.5) and a stale one at
    /// 112.993 (above it, left over from a prior trade). Nearest-to-start alone
    /// grabbed the stale one because its anchor *time* sat nearer the cursor,
    /// baking a floor above the entry that blocked every fill via the at-entry
    /// veto. The side-of-neckline filter drops the above-neckline `too-low`
    /// first, so the real floor wins even though it's anchored further in time.
    #[test]
    fn nearest_to_invalidation_drops_too_low_above_neckline() {
        let (stubs, mcp) = fixture(vec![
            // Neckline ~111.5 (mean of the two anchors).
            (
                stub("neck", "trend_line"),
                drawing("neck", "neckline", vec![(400, 111.5), (500, 111.5)]),
            ),
            // Stale `too-low` ABOVE the neckline, anchored NEAR start (t=590).
            (
                stub("stale", "horizontal_line"),
                drawing("stale", "too-low", vec![(590, 112.993)]),
            ),
            // Real `too-low` floor BELOW the neckline, anchored further (t=450).
            (
                stub("real", "horizontal_line"),
                drawing("real", "too-low", vec![(450, 111.288)]),
            ),
        ]);
        // start=600: by time alone `stale` (|590-600|=10) beats `real`
        // (|450-600|=150) — but `stale` is above the neckline and is dropped.
        let roles = classify(&mcp, &stubs, ANY_RANGE, SlotPref::NearestTo { start: 600 })
            .expect("classify ok");
        assert_eq!(
            roles.invalidation.as_ref().unwrap().id,
            "real",
            "the below-neckline floor wins; the stale above-neckline line is dropped"
        );
        assert_eq!(roles.invalidation_label.as_deref(), Some("too-low"));
    }

    /// THE TIE (Stellar 2026-07-06): a `too-low` (143.1) and a `too-high`
    /// (202.1) both anchored at the **same time**, both on their correct side of
    /// the neckline (200.2) so the side-of-neckline filter keeps both. The
    /// time-distance tiebreak is a dead tie; without a secondary key `min_by`
    /// picks by vec order (too-low → long, the wrong direction). The level
    /// tiebreak picks the line closest to the neckline — too-high (Δ1.9) over
    /// too-low (Δ57.1) → short, the intended trade.
    #[test]
    fn nearest_to_invalidation_time_tie_breaks_on_level_closest_to_neckline() {
        let (stubs, mcp) = fixture(vec![
            // Neckline ~200.2 (mean of the two anchors).
            (
                stub("neck", "trend_line"),
                drawing("neck", "neckline", vec![(400, 200.2), (500, 200.2)]),
            ),
            // too-low FAR below the neckline, same anchor time as too-high.
            (
                stub("low", "horizontal_line"),
                drawing("low", "too-low", vec![(450, 143.1)]),
            ),
            // too-high just ABOVE the neckline, same anchor time.
            (
                stub("high", "horizontal_line"),
                drawing("high", "too-high", vec![(450, 202.1)]),
            ),
        ]);
        // start=600: both invalidations |450-600|=150 — an exact time tie.
        // Level breaks it: |202.1-200.2|=1.9 < |143.1-200.2|=57.1 → `high`.
        let roles = classify(&mcp, &stubs, ANY_RANGE, SlotPref::NearestTo { start: 600 })
            .expect("classify ok");
        assert_eq!(
            roles.invalidation.as_ref().unwrap().id,
            "high",
            "the neckline-hugging cap wins the time-tie; the far stale floor loses"
        );
        assert_eq!(roles.invalidation_label.as_deref(), Some("too-high"));
    }

    /// Mirror for a short: a stale `too-high` *below* the neckline is dropped so
    /// the real cap above it wins, even when the stale one is nearer in time.
    #[test]
    fn nearest_to_invalidation_drops_too_high_below_neckline() {
        let (stubs, mcp) = fixture(vec![
            (
                stub("neck", "trend_line"),
                drawing("neck", "neckline", vec![(400, 111.5), (500, 111.5)]),
            ),
            // Stale `too-high` BELOW the neckline, near start.
            (
                stub("stale", "horizontal_line"),
                drawing("stale", "too-high", vec![(590, 110.2)]),
            ),
            // Real `too-high` cap ABOVE the neckline, further in time.
            (
                stub("real", "horizontal_line"),
                drawing("real", "too-high", vec![(450, 112.1)]),
            ),
        ]);
        let roles = classify(&mcp, &stubs, ANY_RANGE, SlotPref::NearestTo { start: 600 })
            .expect("classify ok");
        assert_eq!(roles.invalidation.as_ref().unwrap().id, "real");
        assert_eq!(roles.invalidation_label.as_deref(), Some("too-high"));
    }

    /// When *every* candidate is on the "wrong" side (an unusual chart, or a
    /// mis-resolved neckline), the filter must not strand the setup — it falls
    /// back to the full set and nearest-to-start still picks one.
    #[test]
    fn nearest_to_invalidation_side_filter_falls_back_when_all_wrong_side() {
        let (stubs, mcp) = fixture(vec![
            (
                stub("neck", "trend_line"),
                drawing("neck", "neckline", vec![(400, 111.5), (500, 111.5)]),
            ),
            // Both `too-low` lines above the neckline (would all be dropped).
            (
                stub("a", "horizontal_line"),
                drawing("a", "too-low", vec![(590, 112.9)]),
            ),
            (
                stub("b", "horizontal_line"),
                drawing("b", "too-low", vec![(450, 112.5)]),
            ),
        ]);
        // Filter would empty the set → keep all → nearest-to-start (t=590) wins.
        let roles = classify(&mcp, &stubs, ANY_RANGE, SlotPref::NearestTo { start: 600 })
            .expect("classify ok");
        assert_eq!(roles.invalidation.as_ref().unwrap().id, "a");
    }

    /// The trade-expiry vertical sits forward of the setup, so `--start` picks
    /// the nearest vertical **at-or-after** the cursor — a stale past expiry to
    /// the left is skipped.
    #[test]
    fn nearest_to_trade_expiry_picks_nearest_after_start() {
        let (stubs, mcp) = fixture(vec![
            (
                stub("past", "vertical_line"),
                drawing("past", "trade-expiry", vec![(200, 0.0)]),
            ),
            (
                stub("future", "vertical_line"),
                drawing("future", "trade-expiry", vec![(800, 0.0)]),
            ),
        ]);
        let roles = classify(&mcp, &stubs, ANY_RANGE, SlotPref::NearestTo { start: 600 })
            .expect("classify ok");
        assert_eq!(roles.trade_expiry.unwrap().id, "future");
    }

    /// The M/W path whose two **shoulders bracket** the cursor
    /// (`B ≤ start ≤ D`) wins; a path that ended before the cursor is a prior
    /// setup and is skipped even though it's older-but-complete.
    #[test]
    fn nearest_to_mw_path_brackets_start() {
        // path anchors: [A run-up-start, B left-shoulder, C neckline, D right-shoulder]
        let (stubs, mcp) = fixture(vec![
            (
                stub("earlier", "path"),
                drawing(
                    "earlier",
                    "",
                    vec![(10, 1.0), (20, 1.1), (30, 1.05), (40, 1.1)],
                ),
            ),
            (
                stub("bracketing", "path"),
                drawing(
                    "bracketing",
                    "",
                    vec![(500, 1.0), (550, 1.1), (600, 1.05), (700, 1.1)],
                ),
            ),
        ]);
        // start=620: only `bracketing` has B(550) ≤ 620 ≤ D(700).
        let roles = classify(&mcp, &stubs, ANY_RANGE, SlotPref::NearestTo { start: 620 })
            .expect("classify ok");
        assert_eq!(roles.mw_path.unwrap().id, "bracketing");
    }

    /// A 3-anchor M/W path (no drawn right shoulder) brackets `start` when the
    /// cursor is at-or-after the left shoulder (`start ≥ B`) — the right
    /// shoulder is still forming.
    #[test]
    fn nearest_to_mw_path_three_anchor_relaxes_to_after_left_shoulder() {
        let (stubs, mcp) = fixture(vec![(
            stub("forming", "path"),
            drawing("forming", "", vec![(500, 1.0), (550, 1.1), (600, 1.05)]),
        )]);
        // start=580 ≥ B(550), no D → qualifies.
        let roles = classify(&mcp, &stubs, ANY_RANGE, SlotPref::NearestTo { start: 580 })
            .expect("classify ok");
        assert_eq!(roles.mw_path.unwrap().id, "forming");
    }

    /// THE BUG (AUD/JPY 2026-06-29): a stray 3-anchor M/W path from an earlier
    /// setup sits far to the left of `start`; the operator is journaling an H&S
    /// whose neckline is drawn much nearer the cursor. The relax rule
    /// (`start >= B`) makes the old path *bracket* start, and a path's mere
    /// presence routes the whole arm to M/W — hijacking the H&S. With the drawn
    /// neckline anchored nearer `start` than the path's own neckline (C), the
    /// path is dropped and the arm stays H&S (`mw_path == None`).
    #[test]
    fn nearest_to_drops_stray_path_when_hs_neckline_is_nearer_start() {
        let (stubs, mcp) = fixture(vec![
            // Stray M/W path from a prior setup, neckline C at t=300, far left.
            (
                stub("stray", "path"),
                drawing("stray", "", vec![(100, 1.0), (200, 1.1), (300, 1.05)]),
            ),
            // The H&S neckline the operator is actually journaling, near start.
            (
                stub("neck", "trend_line"),
                drawing("neck", "neckline", vec![(850, 1.2), (900, 1.2)]),
            ),
        ]);
        // start=950: path C(300) is |650| away; neckline(900) is |50| away → H&S.
        let roles = classify(&mcp, &stubs, ANY_RANGE, SlotPref::NearestTo { start: 950 })
            .expect("classify ok");
        assert!(
            roles.mw_path.is_none(),
            "stray path far from start must not hijack the H&S arm"
        );
        assert_eq!(
            roles.break_and_close.as_ref().unwrap().id,
            "neck",
            "the drawn H&S neckline drives the arm"
        );
    }

    /// Mirror of the above: when the M/W path's own neckline (C) is the one
    /// nearer `start`, the path wins even though an H&S-style trend line is also
    /// present — a genuine M/W journaling arm is not accidentally demoted.
    #[test]
    fn nearest_to_keeps_path_when_it_is_nearer_start_than_neckline() {
        let (stubs, mcp) = fixture(vec![
            // A trend line far to the left (stale), C-equivalent at t=200.
            (
                stub("old-neck", "trend_line"),
                drawing("old-neck", "neckline", vec![(150, 1.2), (200, 1.2)]),
            ),
            // The M/W path being journaled, neckline C at t=900, near start.
            (
                stub("path", "path"),
                drawing("path", "", vec![(700, 1.0), (800, 1.1), (900, 1.05)]),
            ),
        ]);
        // start=950: path C(900) is |50| away; neckline(200) is |750| → M/W wins.
        let roles = classify(&mcp, &stubs, ANY_RANGE, SlotPref::NearestTo { start: 950 })
            .expect("classify ok");
        assert_eq!(
            roles.mw_path.as_ref().unwrap().id,
            "path",
            "the near M/W path drives the arm over a stale trend line"
        );
    }

    /// Under `--start` the news/blackout vertical pairs are scoped to
    /// `[start, trade-expiry]` — a pair before `start` or after the expiry is a
    /// stale/irrelevant leftover and is dropped; only a pair inside the trade's
    /// own lifetime survives.

    #[test]
    fn latest_wins_drops_out_of_window_drawing_even_when_newer() {
        // THE BUG (CAD/JPY): live arming (LatestWins) was grabbing a recent
        // off-screen neckline (t=900..1000) over the in-view one (t=100..200)
        // purely because it was newer. With the visible-window filter applied
        // in *both* modes, the out-of-window drawing is dropped before the
        // tiebreak and the in-view neckline wins.
        let (stubs, mcp) = fixture(vec![
            (
                stub("hist", "trend_line"),
                drawing("hist", "neckline", vec![(100, 1.0), (200, 1.0)]),
            ),
            (
                stub("recent", "trend_line"),
                drawing("recent", "neckline", vec![(900, 1.5), (1000, 1.5)]),
            ),
        ]);
        let view = (50, 300);
        let roles = classify(&mcp, &stubs, view, SlotPref::LatestWins).expect("ok");
        assert_eq!(roles.break_and_close.unwrap().id, "hist");
    }

    #[test]
    fn latest_wins_picks_newest_among_in_window_drawings() {
        // When several drawings are *all* in-window, LatestWins still keeps the
        // newest — the window filter only removes off-screen clutter, it doesn't
        // change the in-window tiebreak.
        let (stubs, mcp) = fixture(vec![
            (
                stub("older", "trend_line"),
                drawing("older", "neckline", vec![(100, 1.0), (150, 1.0)]),
            ),
            (
                stub("newer", "trend_line"),
                drawing("newer", "neckline", vec![(200, 1.5), (250, 1.5)]),
            ),
        ]);
        let view = (50, 300);
        let roles = classify(&mcp, &stubs, view, SlotPref::LatestWins).expect("ok");
        assert_eq!(roles.break_and_close.unwrap().id, "newer");
    }

    #[test]
    fn window_aware_also_applies_to_invalidation_and_trade_expiry() {
        // The same in-window preference holds for the labelled invalidation
        // role and the trade-expiry vertical line, not just trend lines.
        let (stubs, mcp) = fixture(vec![
            // Invalidation: in-window (t=150) vs recent (t=950).
            (
                stub("inv-hist", "horizontal_line"),
                drawing("inv-hist", "too-high", vec![(150, 1.25)]),
            ),
            (
                stub("inv-recent", "horizontal_line"),
                drawing("inv-recent", "too-high", vec![(950, 1.30)]),
            ),
            // Trade-expiry: in-window (t=250) vs recent (t=990).
            (
                stub("exp-hist", "vertical_line"),
                drawing("exp-hist", "trade-expiry", vec![(250, 1.0)]),
            ),
            (
                stub("exp-recent", "vertical_line"),
                drawing("exp-recent", "trade-expiry", vec![(990, 1.0)]),
            ),
        ]);
        let view = (50, 300);
        let roles = classify(&mcp, &stubs, view, SlotPref::WindowAware(view)).expect("ok");
        assert_eq!(roles.invalidation.as_ref().unwrap().id, "inv-hist");
        assert_eq!(roles.invalidation_label.as_deref(), Some("too-high"));
        assert_eq!(roles.trade_expiry.as_ref().unwrap().id, "exp-hist");
    }

    #[test]
    fn pause_resume_labels_are_no_longer_classified() {
        // `pause`/`resume` (and `blackout-start`/`blackout-end`) verticals used
        // to classify into a blackout pair. Windows now come from the calendar,
        // so these labels are ignored — a stray drawn line arms nothing.
        let (stubs, mcp) = fixture(vec![
            (
                stub("ps", "vertical_line"),
                drawing("ps", "PAUSE", vec![(300, 1.0)]),
            ),
            (
                stub("pe", "vertical_line"),
                drawing("pe", "resume", vec![(350, 1.0)]),
            ),
        ]);
        let roles = classify(&mcp, &stubs, ANY_RANGE, SlotPref::LatestWins).expect("ok");
        assert!(roles.blackout_pairs.is_empty());
    }

    #[test]
    fn unknown_label_is_ignored() {
        // A trend line with no recognized label shouldn't show up in
        // any role.
        let (stubs, mcp) = fixture(vec![(
            stub("a", "trend_line"),
            drawing("a", "scratchpad", vec![(50, 1.0), (100, 1.0)]),
        )]);
        let roles = classify(&mcp, &stubs, ANY_RANGE, SlotPref::LatestWins).expect("ok");
        assert!(roles.break_and_close.is_none());
        assert!(roles.retest.is_none());
    }

    #[test]
    fn multiple_sr_levels_all_kept() {
        let (stubs, mcp) = fixture(vec![
            (
                stub("s1", "horizontal_line"),
                drawing("s1", "support", vec![(100, 1.05)]),
            ),
            (
                stub("s2", "horizontal_line"),
                drawing("s2", "resistance", vec![(200, 1.20)]),
            ),
        ]);
        let roles = classify(&mcp, &stubs, ANY_RANGE, SlotPref::LatestWins).expect("ok");
        assert_eq!(roles.sr_levels.len(), 2);
    }

    #[test]
    fn prep_expiry_line_classifies_to_canonical_step() {
        // A `break-and-close-expiry` vertical line lands in prep_expiries
        // resolved to the canonical step name; a `trade-expiry` line on
        // the same chart stays a trade_expiry (no collision).
        let (stubs, mcp) = fixture(vec![
            (
                stub("bnce", "vertical_line"),
                drawing("bnce", "break-and-close-expiry", vec![(600, 1.0)]),
            ),
            (
                stub("exp", "vertical_line"),
                drawing("exp", "trade-expiry", vec![(900, 1.0)]),
            ),
        ]);
        let roles = classify(&mcp, &stubs, ANY_RANGE, SlotPref::LatestWins).expect("ok");
        assert_eq!(roles.prep_expiries.len(), 1);
        assert_eq!(roles.prep_expiries[0].0, "break-and-close");
        assert_eq!(roles.prep_expiries[0].1.id, "bnce");
        assert_eq!(roles.trade_expiry.as_ref().unwrap().id, "exp");
    }

    #[test]
    fn duplicate_prep_expiry_keeps_latest() {
        let (stubs, mcp) = fixture(vec![
            (
                stub("old", "vertical_line"),
                drawing("old", "retest-expiry", vec![(100, 1.0)]),
            ),
            (
                stub("new", "vertical_line"),
                drawing("new", "retest-expiry", vec![(500, 1.0)]),
            ),
        ]);
        let roles = classify(&mcp, &stubs, ANY_RANGE, SlotPref::LatestWins).expect("ok");
        assert_eq!(roles.prep_expiries.len(), 1);
        assert_eq!(roles.prep_expiries[0].0, "retest");
        assert_eq!(roles.prep_expiries[0].1.id, "new");
    }

    #[test]
    fn fib_with_text_label_still_classified() {
        // Fibs match purely on kind — any text label is ignored.
        let (stubs, mcp) = fixture(vec![(
            stub("f", "fib_retracement"),
            drawing("f", "leftover note", vec![(50, 1.2), (200, 1.1)]),
        )]);
        let roles = classify(&mcp, &stubs, ANY_RANGE, SlotPref::LatestWins).expect("ok");
        assert!(roles.tp_fib.is_some());
    }

    #[test]
    fn three_anchor_path_in_range_is_mw() {
        // A 3-anchor path, all anchors inside the visible window, with
        // no label — accepted as the M/W marker.
        let (stubs, mcp) = fixture(vec![(
            stub("p", "path"),
            drawing("p", "", vec![(100, 1.10), (200, 1.12), (300, 1.112)]),
        )]);
        let roles = classify(&mcp, &stubs, (50, 400), SlotPref::LatestWins).expect("ok");
        assert_eq!(roles.mw_path.as_ref().unwrap().id, "p");
        // Anchors preserved in draw order = A, B, C.
        let pts = &roles.mw_path.unwrap().points;
        assert_eq!(pts[0].price, 1.10);
        assert_eq!(pts[2].price, 1.112);
    }

    #[test]
    fn path_partly_off_screen_is_ignored() {
        // Last anchor sits past the visible window → not the live
        // setup, ignore it (stale scrolled-away path).
        let (stubs, mcp) = fixture(vec![(
            stub("p", "path"),
            drawing("p", "", vec![(100, 1.10), (200, 1.12), (900, 1.112)]),
        )]);
        let roles = classify(&mcp, &stubs, (50, 400), SlotPref::LatestWins).expect("ok");
        assert!(roles.mw_path.is_none());
    }

    #[test]
    fn two_anchor_path_is_ignored() {
        let (stubs, mcp) = fixture(vec![(
            stub("p", "path"),
            drawing("p", "", vec![(100, 1.10), (200, 1.12)]),
        )]);
        let roles = classify(&mcp, &stubs, ANY_RANGE, SlotPref::LatestWins).expect("ok");
        assert!(roles.mw_path.is_none());
    }

    #[test]
    fn four_anchor_path_is_the_right_shoulder_form() {
        // A 4-anchor path is the 4-point M/W form (D = right shoulder);
        // it classifies as an mw_path with all four points preserved.
        let (stubs, mcp) = fixture(vec![(
            stub("p", "path"),
            drawing(
                "p",
                "",
                vec![(100, 1.1), (200, 1.12), (300, 1.11), (400, 1.13)],
            ),
        )]);
        let roles = classify(&mcp, &stubs, ANY_RANGE, SlotPref::LatestWins).expect("ok");
        let path = roles.mw_path.expect("4-anchor path is a valid mw_path");
        assert_eq!(path.points.len(), 4);
    }

    #[test]
    fn five_anchor_path_is_ignored() {
        // 5+ anchors is a fat-fingered shape → ignored.
        let (stubs, mcp) = fixture(vec![(
            stub("p", "path"),
            drawing(
                "p",
                "",
                vec![
                    (100, 1.1),
                    (200, 1.12),
                    (300, 1.11),
                    (400, 1.13),
                    (500, 1.1),
                ],
            ),
        )]);
        let roles = classify(&mcp, &stubs, ANY_RANGE, SlotPref::LatestWins).expect("ok");
        assert!(roles.mw_path.is_none());
    }

    #[test]
    fn duplicate_mw_paths_keep_latest() {
        // Two valid 3-anchor paths in range; the one with the later
        // anchor time wins (older is a stale leftover).
        let (stubs, mcp) = fixture(vec![
            (
                stub("old", "path"),
                drawing("old", "", vec![(100, 1.1), (150, 1.12), (200, 1.112)]),
            ),
            (
                stub("new", "path"),
                drawing("new", "", vec![(600, 1.1), (650, 1.12), (700, 1.112)]),
            ),
        ]);
        let roles = classify(&mcp, &stubs, (50, 800), SlotPref::LatestWins).expect("ok");
        assert_eq!(roles.mw_path.unwrap().id, "new");
    }

    #[test]
    fn short_position_classifies_with_direction() {
        let (stubs, mcp) = fixture(vec![(
            stub("sp", "short_position"),
            position("sp", 23475.0, 1773738000, 3000.0, 7007.0),
        )]);
        let roles = classify(&mcp, &stubs, ANY_RANGE, SlotPref::LatestWins).expect("ok");
        let pos = roles.position.expect("position present");
        assert_eq!(pos.direction, PositionDirection::Short);
        assert_eq!(pos.drawing.points[0].price, 23475.0);
        assert_eq!(pos.drawing.properties.stop_level, Some(3000.0));
        assert_eq!(pos.drawing.properties.profit_level, Some(7007.0));
    }

    #[test]
    fn long_position_classifies_with_direction() {
        let (stubs, mcp) = fixture(vec![(
            stub("lp", "long_position"),
            position("lp", 24195.3, 1778702400, 801.0, 2223.0),
        )]);
        let roles = classify(&mcp, &stubs, ANY_RANGE, SlotPref::LatestWins).expect("ok");
        assert_eq!(roles.position.unwrap().direction, PositionDirection::Long);
    }

    #[test]
    fn half_drawn_position_without_levels_is_ignored() {
        // A position tool missing its profit level isn't a complete
        // setup — ignore it rather than arm a TP-less entry.
        let half = Drawing {
            id: "h".into(),
            points: vec![Point {
                time: 100,
                price: 100.0,
            }],
            properties: Properties {
                text: None,
                stop_level: Some(50.0),
                profit_level: None,
                qty: None,
            },
        };
        let (stubs, mcp) = fixture(vec![(stub("h", "short_position"), half)]);
        let roles = classify(&mcp, &stubs, ANY_RANGE, SlotPref::LatestWins).expect("ok");
        assert!(roles.position.is_none());
    }

    #[test]
    fn cadjpy_repro_in_window_neckline_wins_over_off_screen_pair() {
        // Real session, CAD/JPY H1. Visible window May 15 → May 27.
        //   visible_range = 1778810400 .. 1779843600
        // Four trend lines on the chart; only `1kUSW4` (May 18 → May 20) sits
        // inside the window. The June pair (`2Xfe1I`/`7rdwbe`) and the April
        // line (`sh1OWn`) are off-screen leftovers. Live arming (LatestWins)
        // used to grab the June pair because it was newest. The window filter
        // now collapses break_and_close to the single in-view neckline.
        let view = (1778810400, 1779843600);
        // May 18 23:00 → May 20 19:00 UTC (inside the window).
        let correct = (1779145200_i64, 1779303600_i64);
        // April 13 → 14 (left of view).
        let april = (1776038400_i64, 1776124800_i64);
        // June 3 → 4 (right of view), drawn twice.
        let june = (1780531200_i64, 1780617600_i64);
        let (stubs, mcp) = fixture(vec![
            (
                stub("1kUSW4", "trend_line"),
                drawing(
                    "1kUSW4",
                    "neckline",
                    vec![(correct.0, 115.502), (correct.1, 115.46)],
                ),
            ),
            (
                stub("sh1OWn", "trend_line"),
                drawing(
                    "sh1OWn",
                    "neckline",
                    vec![(april.0, 110.0), (april.1, 110.0)],
                ),
            ),
            (
                stub("2Xfe1I", "trend_line"),
                drawing("2Xfe1I", "neckline", vec![(june.0, 120.0), (june.1, 120.0)]),
            ),
            (
                stub("7rdwbe", "trend_line"),
                drawing("7rdwbe", "neckline", vec![(june.0, 121.0), (june.1, 121.0)]),
            ),
        ]);
        let roles = classify(&mcp, &stubs, view, SlotPref::LatestWins).expect("ok");
        let neck = roles.break_and_close.expect("break_and_close resolved");
        assert_eq!(neck.id, "1kUSW4");
        // Anchored at the May setup, not June/April.
        assert_eq!(neck.earliest_time(), correct.0);
        assert_eq!(neck.latest_time(), correct.1);
    }

    #[test]
    fn trend_line_straddling_the_whole_view_is_kept() {
        // One anchor left of `from`, one right of `to` — the line spans the
        // entire visible window, so intersection keeps it even though neither
        // anchor is inside. (Containment would have wrongly dropped it.)
        let (stubs, mcp) = fixture(vec![(
            stub("span", "trend_line"),
            drawing("span", "neckline", vec![(50, 1.0), (500, 1.0)]),
        )]);
        let view = (100, 200);
        let roles = classify(&mcp, &stubs, view, SlotPref::LatestWins).expect("ok");
        assert_eq!(roles.break_and_close.unwrap().id, "span");
    }

    #[test]
    fn single_partly_off_screen_drawing_is_kept_by_intersection() {
        // Exactly one neckline, overlapping only the left edge of the window
        // (t=50..150 vs view 100..300). Intersection keeps it; we must not
        // regress to "nothing resolved" for a partly-off-screen sole drawing.
        let (stubs, mcp) = fixture(vec![(
            stub("edge", "trend_line"),
            drawing("edge", "neckline", vec![(50, 1.0), (150, 1.0)]),
        )]);
        let view = (100, 300);
        let roles = classify(&mcp, &stubs, view, SlotPref::LatestWins).expect("ok");
        assert_eq!(roles.break_and_close.unwrap().id, "edge");
    }

    #[test]
    fn trade_expiry_just_past_right_edge_is_kept_and_nearest_wins() {
        // The expiry marker legitimately sits at/after the right edge. The
        // generic window filter would drop it; `pick_trade_expiry` keeps any
        // expiry within a forward margin of `to` and prefers the one nearest
        // the right edge. Here view is [100, 300] (width 200, margin to 500):
        //   - `inview`  at t=250 (inside)            -> qualifies
        //   - `just`    at t=350 (just past `to`)    -> qualifies, nearest edge
        //   - `wayfar`  at t=900 (beyond the margin) -> dropped
        let (stubs, mcp) = fixture(vec![
            (
                stub("inview", "vertical_line"),
                drawing("inview", "trade-expiry", vec![(250, 1.0)]),
            ),
            (
                stub("just", "vertical_line"),
                drawing("just", "trade-expiry", vec![(350, 1.0)]),
            ),
            (
                stub("wayfar", "vertical_line"),
                drawing("wayfar", "trade-expiry", vec![(900, 1.0)]),
            ),
        ]);
        let view = (100, 300);
        let roles = classify(&mcp, &stubs, view, SlotPref::LatestWins).expect("ok");
        // Nearest the right edge among qualifiers (the just-past-`to` one).
        assert_eq!(roles.trade_expiry.unwrap().id, "just");
    }

    #[test]
    fn trade_expiry_off_screen_left_is_dropped() {
        // A stale expiry well to the left of the window must not be chosen over
        // the real one inside it.
        let (stubs, mcp) = fixture(vec![
            (
                stub("stale", "vertical_line"),
                drawing("stale", "trade-expiry", vec![(10, 1.0)]),
            ),
            (
                stub("real", "vertical_line"),
                drawing("real", "trade-expiry", vec![(250, 1.0)]),
            ),
        ]);
        let view = (100, 300);
        let roles = classify(&mcp, &stubs, view, SlotPref::LatestWins).expect("ok");
        assert_eq!(roles.trade_expiry.unwrap().id, "real");
    }

    #[test]
    fn duplicate_positions_keep_latest() {
        let (stubs, mcp) = fixture(vec![
            (
                stub("old", "short_position"),
                position("old", 100.0, 100, 30.0, 70.0),
            ),
            (
                stub("new", "long_position"),
                position("new", 200.0, 900, 40.0, 80.0),
            ),
        ]);
        let roles = classify(&mcp, &stubs, ANY_RANGE, SlotPref::LatestWins).expect("ok");
        let pos = roles.position.unwrap();
        assert_eq!(pos.drawing.id, "new");
        assert_eq!(pos.direction, PositionDirection::Long);
    }
}
