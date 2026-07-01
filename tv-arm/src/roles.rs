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
//! Blackout and news vertical-line pairs are collected as multi-slot
//! lists and chronologically paired via
//! [`trading_view::pair_lines::pair_vertical_lines`]; an odd count or a
//! reversed pair is a hard error so a misdrawn chart can't silently
//! arm half a window.

use color_eyre::eyre::Result;
use tracing::{debug, info, warn};
use trade_control_conventions::{
    BLACKOUT_END_LABELS, BLACKOUT_START_LABELS, BREAK_LABELS, INVALIDATION_LABELS, NEWS_END_LABELS,
    NEWS_START_LABELS, RETEST_LABELS, SR_LEVEL_LABELS, TRADE_EXPIRY_LABELS, matches,
    prep_name_from_expiry_label,
};

use trading_view::drawings::{Drawing, DrawingStub};
use trading_view::mcp::TvMcp;
use trading_view::pair_lines::{TimedAnchor, pair_vertical_lines};

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
    /// Chronologically paired blackout (pause/resume) windows.
    pub blackout_pairs: Vec<(Drawing, Drawing)>,
    /// Chronologically paired news-window pairs.
    pub news_pairs: Vec<(Drawing, Drawing)>,
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
    let mut retest_lines: Vec<Drawing> = Vec::new();
    let mut tp_fibs: Vec<Drawing> = Vec::new();
    let mut trade_expiries: Vec<Drawing> = Vec::new();
    let mut blackout_starts: Vec<Drawing> = Vec::new();
    let mut blackout_ends: Vec<Drawing> = Vec::new();
    let mut news_starts: Vec<Drawing> = Vec::new();
    let mut news_ends: Vec<Drawing> = Vec::new();
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
            kind::TREND_LINE if matches(lbl, RETEST_LABELS) => {
                retest_lines.push(d);
                Some("retest")
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
            kind::VERTICAL_LINE if matches(lbl, BLACKOUT_START_LABELS) => {
                blackout_starts.push(d);
                Some("blackout_start")
            }
            kind::VERTICAL_LINE if matches(lbl, BLACKOUT_END_LABELS) => {
                blackout_ends.push(d);
                Some("blackout_end")
            }
            kind::VERTICAL_LINE if matches(lbl, NEWS_START_LABELS) => {
                news_starts.push(d);
                Some("news_start")
            }
            kind::VERTICAL_LINE if matches(lbl, NEWS_END_LABELS) => {
                news_ends.push(d);
                Some("news_end")
            }
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

    if let Some((d, lbl)) =
        pick_slot_with_label(invalidations, "invalidation", visible_range, slot_pref)
    {
        roles.invalidation = Some(d);
        roles.invalidation_label = Some(lbl);
    }
    roles.break_and_close = pick_slot(break_lines, "break_and_close", visible_range, slot_pref);
    roles.retest = pick_slot(retest_lines, "retest", visible_range, slot_pref);
    roles.tp_fib = pick_slot(tp_fibs, "tp_fib", visible_range, slot_pref);
    roles.trade_expiry = pick_trade_expiry(trade_expiries, visible_range, slot_pref);

    // Pause/resume and news-start/news-end lines that sit outside the
    // visible window are stale leftovers from a prior setup — a window
    // that may already have closed. Pairing them would mint a blackout
    // whose `end_time` is in the past, which `build_pause_from_spec`
    // rejects ("refusing to arm a stale blackout"). Drop off-screen
    // lines up front so only the on-screen window is armed.
    let blackout_starts = in_visible_window(blackout_starts, "blackout_start", visible_range);
    let blackout_ends = in_visible_window(blackout_ends, "blackout_end", visible_range);
    let news_starts = in_visible_window(news_starts, "news_start", visible_range);
    let news_ends = in_visible_window(news_ends, "news_end", visible_range);

    roles.blackout_pairs = pair_vertical_lines(blackout_starts, blackout_ends, "blackout")?;
    roles.news_pairs = pair_vertical_lines(news_starts, news_ends, "news")?;
    roles.sr_levels = sr_levels;
    roles.prep_expiries = latest_prep_expiry_per_step(prep_expiry_lines);
    // M/W paths are already in-window-filtered by `is_mw_path`, so latest-wins
    // among qualifiers is correct in both modes (the window filter inside
    // `pick_slot` is a no-op for them). Under `--start` the containment filter
    // is dropped, so select the path whose two shoulders bracket the cursor.
    roles.mw_path = pick_mw_path(mw_paths, slot_pref);
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

/// Keep only the vertical-line drawings whose anchor sits inside the
/// visible window `[from, to]` (inclusive). Off-screen pause / resume /
/// news lines are stale leftovers from a prior, possibly already-closed
/// window — arming them would mint a blackout with a past `end_time` that
/// `build_pause_from_spec` rejects. Each dropped line is logged so an
/// operator who scrolled a line off-screen can see why it was ignored.
fn in_visible_window(cands: Vec<Drawing>, role: &str, (from, to): (i64, i64)) -> Vec<Drawing> {
    cands
        .into_iter()
        .filter(|d| {
            let t = d.anchor_time();
            let kept = t >= from && t <= to;
            if !kept {
                debug!(
                    role,
                    anchor_time = t,
                    window_from = from,
                    window_to = to,
                    "vertical line ignored — anchor outside visible window",
                );
            }
            kept
        })
        .collect()
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
/// Anchor order (see [`is_mw_path`]): `points[0]=A run-up-start`,
/// `points[1]=B left-shoulder`, `points[2]=C neckline`, `points[3]=D
/// right-shoulder` (optional).
fn pick_mw_path(cands: Vec<Drawing>, pref: SlotPref) -> Option<Drawing> {
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
    let bracketing = cands
        .iter()
        .filter(|d| brackets(d))
        .max_by_key(|d| d.latest_time())
        .cloned();
    if let Some(d) = bracketing {
        return Some(d);
    }
    debug!(
        role = "mw_path",
        start, "no M/W path whose shoulders bracket --start; falling back to latest"
    );
    cands.into_iter().max_by_key(|d| d.latest_time())
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
fn pick_slot_with_label(
    cands: Vec<(Drawing, String)>,
    role: &str,
    window: (i64, i64),
    pref: SlotPref,
) -> Option<(Drawing, String)> {
    if cands.is_empty() {
        return None;
    }
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
    let chosen = if let SlotPref::NearestTo { start } = pref {
        drawings
            .into_iter()
            .min_by_key(|d| (d.anchor_time() - start).abs())?
    } else {
        pick_slot(drawings, role, window, pref)?
    };
    let lbl = labels.get(&chosen.id).cloned().unwrap_or_default();
    Some((chosen, lbl))
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
        // retest, fib, trade-expiry, plus one blackout pair and one
        // news pair and one S/R level.
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
        assert_eq!(roles.retest.as_ref().unwrap().id, "re");
        assert_eq!(roles.tp_fib.as_ref().unwrap().id, "fib");
        assert_eq!(roles.trade_expiry.as_ref().unwrap().id, "exp");

        assert_eq!(roles.blackout_pairs.len(), 1);
        assert_eq!(roles.blackout_pairs[0].0.id, "bs");
        assert_eq!(roles.blackout_pairs[0].1.id, "be");

        assert_eq!(roles.news_pairs.len(), 1);
        assert_eq!(roles.news_pairs[0].0.id, "ns");
        assert_eq!(roles.news_pairs[0].1.id, "ne");

        assert_eq!(roles.sr_levels.len(), 1);
        assert_eq!(roles.sr_levels[0].id, "sr");
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
    fn pause_and_news_lines_outside_visible_window_are_ignored() {
        // Two pause/resume pairs and two news pairs: one of each sits
        // inside the visible window [200, 500], the other is a stale
        // leftover anchored before it. Only the in-window pair survives —
        // off-screen lines would otherwise mint a blackout whose end_time
        // is already in the past and `build_pause_from_spec` would reject.
        let (stubs, mcp) = fixture(vec![
            // Stale (off-screen) blackout pair — anchored well before the window.
            (
                stub("bs_old", "vertical_line"),
                drawing("bs_old", "pause", vec![(10, 1.0)]),
            ),
            (
                stub("be_old", "vertical_line"),
                drawing("be_old", "resume", vec![(20, 1.0)]),
            ),
            // In-window blackout pair.
            (
                stub("bs", "vertical_line"),
                drawing("bs", "blackout-start", vec![(300, 1.0)]),
            ),
            (
                stub("be", "vertical_line"),
                drawing("be", "blackout-end", vec![(350, 1.0)]),
            ),
            // Stale (off-screen) news pair.
            (
                stub("ns_old", "vertical_line"),
                drawing("ns_old", "news-start", vec![(30, 1.0)]),
            ),
            (
                stub("ne_old", "vertical_line"),
                drawing("ne_old", "news-end", vec![(40, 1.0)]),
            ),
            // In-window news pair.
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

        // Exactly the in-window pairs survive; the off-screen lines are dropped.
        assert_eq!(roles.blackout_pairs.len(), 1);
        assert_eq!(roles.blackout_pairs[0].0.id, "bs");
        assert_eq!(roles.blackout_pairs[0].1.id, "be");

        assert_eq!(roles.news_pairs.len(), 1);
        assert_eq!(roles.news_pairs[0].0.id, "ns");
        assert_eq!(roles.news_pairs[0].1.id, "ne");
    }

    #[test]
    fn pause_line_on_window_boundary_is_kept() {
        // Anchors exactly on the window edges [200, 500] are inclusive —
        // a line drawn at the boundary is on-screen and must survive.
        let (stubs, mcp) = fixture(vec![
            (
                stub("bs", "vertical_line"),
                drawing("bs", "pause", vec![(200, 1.0)]),
            ),
            (
                stub("be", "vertical_line"),
                drawing("be", "resume", vec![(500, 1.0)]),
            ),
        ]);
        let roles = classify(&mcp, &stubs, (200, 500), SlotPref::LatestWins).expect("classify ok");
        assert_eq!(roles.blackout_pairs.len(), 1);
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
    fn label_aliases_accepted() {
        // `pause`/`resume` are aliases for `blackout-start`/`blackout-end`.
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
        assert_eq!(roles.blackout_pairs.len(), 1);
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
    fn odd_blackout_count_errors() {
        let (stubs, mcp) = fixture(vec![(
            stub("bs", "vertical_line"),
            drawing("bs", "blackout-start", vec![(300, 1.0)]),
        )]);
        let err = classify(&mcp, &stubs, ANY_RANGE, SlotPref::LatestWins).unwrap_err();
        assert!(format!("{err}").contains("blackout"));
    }

    #[test]
    fn reversed_news_pair_errors() {
        let (stubs, mcp) = fixture(vec![
            (
                stub("ns", "vertical_line"),
                drawing("ns", "news-start", vec![(500, 1.0)]),
            ),
            (
                stub("ne", "vertical_line"),
                drawing("ne", "news-end", vec![(400, 1.0)]),
            ),
        ]);
        let err = classify(&mcp, &stubs, ANY_RANGE, SlotPref::LatestWins).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("news"), "msg = {msg}");
        assert!(msg.contains("reversed"), "msg = {msg}");
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
