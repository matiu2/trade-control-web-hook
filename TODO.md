# TODO

## Done — migrate calendar_bars::parse_instrument to instrument-lookup (v9, 0.2.0)

Bug: `calendar_bars::parse_instrument` (`cli/src/calendar_bars.rs:278`) uses
the legacy `trade_calendar_maker::Instrument::from_oanda_symbol`, which only
knows OANDA forex-style symbols. TradeNation diff/index names (e.g.
`Wall St 30 / Germany 40 Rolling Future Diff`, `US30DE40`) fail, so the
`calendar-bars` step is skipped with a WARN during a tv-arm run. Downstream
the parsed `Instrument` is consumed only via `is_affected_by(currency)` →
only `affected_currencies` matters. `CalendarBarsArgs` already carries the
broker, so resolution can be precise.

Decision (user): **instrument-lookup only** — drop the legacy fallback.

Status: DONE — all green, verified end-to-end.

- [x] Add `instrument-lookup` dep to `cli/Cargo.toml` (path `../../instrument-lookup`).
- [x] Rewrite `parse_instrument(raw, broker)` → resolve via
      `by_broker_symbol(broker, raw)` then `resolve(raw)`; build `Instrument`
      from `asset.news_currencies` + `class`→`InstrumentType`. Hard error if
      it misses (no legacy fallback). `broker_for` + `instrument_type_for`
      helpers; Crypto/Stock → Index.
- [x] Update call site in `run_calendar_bars` to pass `args.broker.into()`.
- [x] Tests: OANDA `EUR_USD`, TN `CHF/JPY`, TN multi-word `Germany 40`
      index, canonical `US30` id, rejects-unknown. 230 cli lib tests pass.
- [x] cargo clippy + cargo fmt clean.
- [x] Verified: `trade-control calendar-bars --instrument "Wall St 30 /
      Germany 40 Rolling Future Diff" --broker tradenation` (the name that
      used to throw `unsupported instrument symbol`) now resolves, kept the
      USD CPI event, wrote pause+news bundles.
- [x] README: no change needed — `calendar-bars` is an internal tv-arm
      sub-step (no README section), and the README already states tv-arm
      resolves chart symbols via instrument-lookup. This makes the internal
      path consistent with the documented behaviour.
- [x] Commit (26fa357) + push; parent submodule pointer advanced.
- [x] Release v9: bump 0.1.0 → 0.2.0 (root + cli; tv-arm/tv-news dep reqs
      bumped to match), CHANGELOG v9, annotated tag `v9` pushed (b75d56b),
      parent pointer re-advanced (23a4ccf).

Note: the original tv-arm chart has since moved to `US30UK100` (a
different diff); user added that overlay entry separately. Out of scope
for this change — the parse_instrument fix covers any catalog-resolvable
instrument, diff or not.

## In progress — M and W (double-top / double-bottom) reversal trades

Adds M (double-top, short) / W (double-bottom, long) as a first-class trade
type alongside H&S. Operator marks 3 points with the PATH tool (runup start
A, first peak/trough B, neckline retrace C). Plan:
`~/.home-claude/plans/we-don-t-have-any-typed-shell.md`.

Key constraint: Pine is chart-attached (instrument-agnostic) so it can't
read path anchors. tv-arm bakes static M/W params into the signed enter
intent + emits cancel/abort as fast level vetos; a per-bar Pine hook pushes
only OHLC; the **worker** computes entry/SL/TP from baked params + shell.

Decisions: neckline gate errors ≥40% (`--allow-50-pct-m-trades` → ≤50%);
mid-correct spread (±½spread on every level); single-shot, only a broker
rejection of a placed order disarms; alignment == the cancel level.

Commits (each tested, clippy+fmt green):
- [x] 1. conventions — MW_PATH_LABELS, mw_direction_from_label; basenames
  VetoMwCancel/VetoMwAbort  (fa8f505)
- [x] 2. tv-arm/src/mw_geometry.rs — neckline_retrace_pct, cancel_level,
  abort_level (temp `None` stub in alert_spec for the two new basenames;
  replaced properly in commit 8)
- [x] 3. core — MwParams + Intent.mw + validate (MwOnNonEnter /
  MwFieldInvalid; `mw: None` threaded through all Intent literals)
- [x] 4. core/src/intent/mw_resolution.rs — mid-correct entry/SL/TP +
  from_intent branch (shared sizing tail factored into
  finish_with_sizing; wrong-side stop → InvalidGeometry = stay armed)
  ⚠️ PLAN CORRECTION (verified live via tv-mcp 2026-06-08, GBP/CAD chart):
  The TV **path tool has NO text/label property** (only lineColor/width/
  style/ends/visible/frozen). So commit 1's `MW_PATH_LABELS` +
  `mw_direction_from_label` are **DEAD for paths**. Detection is now
  GEOMETRY-ONLY. See memory `mw-path-detection-is-geometry-only`.
  Verified: path kind string = `"path"`; anchors come in draw order A,B,C;
  `mcp.get_range() -> ChartRange` already exists at trading-view/src/mcp.rs:113.

- [x] 5. tv-arm roles.rs + mw_geometry.rs — kind::PATH="path"; Roles.mw_path;
  classify a `path` with EXACTLY 3 anchors all inside the live visible range
  (threaded `get_range().visible_range` into `classify(.., view)`; latest-wins).
  Added `mw_direction_from_anchors(A,B)` (A above B → W/long; A below B → M/short)
  + `check_mw_structure(A,B,C)` "first leg longer by price than B→C" gate
  (hard-error w/ A/B/C + leg lengths) to mw_geometry.rs. NO label lookup.
  tv-arm 113 tests green; clippy+fmt clean.
- [x] 6. cli — TradeSpec.mw + build_mw_pattern + 4 builders. DONE.
  Added `MwSpec` (mirrors core MwParams) + `TradeSpec.mw: Option<MwSpec>`
  (serde-elided when None). `build_mw_pattern` emits exactly 4 alerts:
  `build_mw_cancel_alert`/`build_mw_abort_alert` (both Veto/CancelPending),
  reused `build_trade_expiry_alert`, `build_mw_enter_alert` (direction from
  pattern, `mw: Some`, entry/SL/TP all None, vetos=[mw-cancel,mw-abort,
  trade-expiry], empty preps, max_retries:0). Dispatch: `build_trade_from_spec`
  validates mw↔pattern agreement then routes M/W to build_mw_pattern before
  PatternGeometry (no unreachable! consulted); interactive M/W rejects (chart-
  built only). NO 06-close-on-reversal (TP is hard 1R). cli 227 tests green;
  clippy+fmt clean. tv-arm pipeline.rs H&S spec gets `mw: None` (M/W branch is
  commit 9). NOTE for commit 9: interactive build still errors for M/W — the
  `_ => unreachable!` arm in PatternGeometry::for_pattern stays (M/W never
  call it). NOTE: TradePattern::for_pattern still panics for M/W by design.
- [x] 7. Pine — `alertcondition(true, "Every Bar Close", 'close/high/low/time')`
  added to candle-signals-v2.pine (TV built-ins only, no new plots). H&S keeps
  firing on Long/Short Pattern; M/W enter binds to this in commit 8. Added a
  v2.4 header changelog note flagging the **manual republish** requirement.
  README left as-is (no stale M/W claim; operator-facing M/W story lands in
  the final README sync after commits 8–10).
- [x] 8. tv-arm alert_spec — M/W `VetoMwCancel`→PriceValue at `cancel_level`
  (intra-bar `OnFirstFire`); `VetoMwAbort`→PriceValue at `abort_level(neckline)`
  (`OnBarClose`); both read anchors from `roles.mw_path.points = [A,B,C]` via
  new `MwVeto`+`mw_price_veto`. `build_alert_spec` gained an `is_mw` flag:
  Enter binds to `PLOT_EVERY_BAR_CLOSE` (new conventions const `plot_12`) when
  is_mw, else `entry_plot_for(direction)`. pipeline `build_all_payloads`
  derives is_mw from `built_trade.spec.pattern` (M|W). 118 tv-arm tests (5
  new), 33 conventions tests, clippy+fmt clean. NOTE for commit 9: pipeline's
  H&S `build_trade_spec`/`build_trade_from_spec` path is unchanged — M/W never
  reaches build_all_payloads yet (no M/W branch), so is_mw is always false in
  practice until commit 9 wires the M/W pipeline branch. ⚠️ plot_12 is a
  declaration-order ASSUMPTION (next_candle_timestamp plots shifted indices in
  v2.3) — verify on a live chart in the commit-9 dry build; mismatch shows as
  "condition not found" on 05-enter.
  RESOLVED 2026-06-09: the assumption was wrong. The 5 v2.3
  `next_candle_timestamp_1..5` plots sit between `recent_low` (plot_9) and the
  alertconditions, so the two pattern alertconditions are `plot_15`/`plot_16`
  and Every Bar Close is `plot_17` — not 10/11/12. A live `tv-arm` run failed
  05-enter + 06-close-on-reversal with `err.code="general"` (the catch-all TV
  returns when an alertcondition plot index doesn't resolve). Fixed the three
  conventions consts + tests.
  FOLLOW-UP 2026-06-09: eliminated the whole failure class. Alertconditions
  are now bound by **title** (`"Long Pattern"`/`"Short Pattern"`/`"Every Bar
  Close"`) instead of positional `plot_N`. conventions exposes
  ALERT_*_PATTERN/ALERT_EVERY_BAR_CLOSE + entry_alert_for/reversal_close_alert_for
  (the PLOT_*/entry_plot_for/reversal_close_plot_for consts are gone); the
  AlertPayload field is `alert_cond_title`; the tv-arm JS template resolves
  title → live plot_N from the study's `metaInfo().plots`(type=alertcondition)
  + `metaInfo().styles[id].title`, failing loudly if the title is absent. Plot
  reordering can no longer break the binding. Verified the live resolver maps
  the 3 titles to plot_15/16/17 on the real chart.
- [x] 9. tv-arm args+pipeline. args.rs: --allow-50-pct-m-trades, --spread-pips
  (temp, Option), --pip-size (temp, default 0.0001). pipeline.rs: `run` step 3
  now dispatches on `roles.mw_path.is_some()` to `resolve_mw_trade` vs
  `resolve_hs_trade` (H&S logic lifted unchanged into a resolver). New
  `ResolveError{Reject,Fatal}`: Reject→print+exit1, Fatal→propagate. M/W
  resolver: 3-anchor guard, trade_expiry required, --spread-pips required,
  direction from mw_direction_from_anchors, check_mw_structure,
  gate_neckline_pct (≥40% w/o flag errors, ≤50% with, >50% always, NaN errors),
  build_mw_trade_spec (no preps, max_retries 0, mw baked, no SR/news close).
  cli: exported MwSpec. Bumped instrument-lookup dep 0.1→0.2 in tv-arm AND
  tv-news Cargo.toml (the pip-size agent bumped that crate to 0.2.0). 129
  tv-arm tests (12 new), clippy+fmt clean; cli green. NOTE: the M/W enter still
  bakes args.pip_size (default 0.0001) — wiring it to read instrument-lookup's
  new tick_size is the pip-size project below, NOT done here.
- [x] 10. tv-arm — live broker spread read (replaces required --spread-pips).
  New `tv-arm/src/spread.rs`: `read_spread_pips(broker, instrument, pip_size)`
  reads live bid/ask and returns spread/pip_size. OANDA via `get_pricing`
  (token from OANDA_TOKEN|OANDA_API_KEY, first account from `get_accounts` —
  spread is account-agnostic; `PriceTick::best_bid/best_ask`). TradeNation via
  `TradeNationClient::new_demo().resolve_market(name)` → market_id, then the
  unauthenticated `ohlcv::latest_bid_ask(&reqwest::Client, market_id)`.
  Non-finite / zero / inverted spread = hard error (market closed / stale feed).
  **No operator override** — `--spread-pips` REMOVED entirely; a failed read
  aborts the arm (user decision: "read from broker or hard fail"). pipeline.rs:
  `resolve_mw_trade` (live wrapper) runs cheap `check_mw_required` guards first,
  then `read_spread_blocking` (short-lived tokio rt like auto-draw), delegates to
  pure `resolve_mw_trade_with_spread(.., spread_pips)` (unit-tested w/ injected
  SPREAD const). Deps added to tv-arm: oanda-client (path), tradenation-api (git
  tag, native/no-wasm), reqwest 0.12 (rustls-tls, matches ecosystem). 137 tv-arm
  tests (6 new spread + check_mw_required tests; dropped obsolete
  requires_spread_pips), clippy+fmt clean; root worker (wasm lib) still checks.
- [x] README sync. Added an **M/W bundle** table to "Alert basenames"
  (01-veto-mw-cancel intra-bar OnFirstFire / 01-veto-mw-abort OnBarClose,
  both cancel-pending; 02-veto-trade-expiry; 05-enter bound to the Pine
  "Every Bar Close" alertcondition with the baked `mw:` block; no
  06-close-on-reversal since TP is a hard 1R). Added an **M/W setups**
  subsection to "Chart-driven arming: tv-arm": the 3-anchor PATH tool
  (A runup-start / B first peak-trough / C neckline, draw order),
  geometry-only direction (A>B → W/long, A<B → M/short; no label), the
  neckline-depth gate + --allow-50-pct-m-trades, the live broker spread
  read (OANDA /pricing needs OANDA_TOKEN|OANDA_API_KEY; TN chart bid/ask;
  no override, hard-fail), no prep chain / max_retries 0. Also fixed a
  stale H&S CLI comment ("pair with --dry-run" → "omit --create-alerts to
  only write to disk"; tv-arm has no --dry-run).
- [~] pip-size everywhere (separate project). DONE in instrument-lookup:
  tick_size + decimal_places baked v0.2.0 (TradeNation-sourced, 1223 API /
  96 class-default), authoritative `pip_size` field being added now (agent,
  → v0.3.0). KEY: pip_size != tick_size — fractional-pip FX (5dp/3dp) quotes
  10× finer than a pip; gold/index pip == tick. Read `asset.pip_size`, never
  re-derive. Consumer migration scope:
    - tv-arm: DONE (v0.3.0 committed: EURUSD 0.0001, USDJPY/gold 0.01,
      JP225 1.0). Bakes `resolved.asset.pip_size`; --pip-size is now Option
      (None=catalog) and overrides only when set. Dep 0.2->0.3 in tv-arm +
      tv-news. 131 tests (+2: catalog-pip-baked, flag-overrides).
    - worker `src/lib.rs::pip_size_for`: NOT migrating now. Worker is WASM and
      reads pip from the instrument *string*, not an Asset; secret+0.0001
      default path already works. Adding catalog resolution per-alert is a
      separate, riskier change. Follow-up.
    - cli `script_validator.rs:57` (hardcoded 0.0001): sign-time validation
      only, low value. Follow-up.

Checkpoint tag (current HEAD efa38ff): `pre-m-and-w-trades`.

## Done — bar-based pending-order expiry (`expiry_bars`)

Cancel a resting stop/limit order N bars (1..=5) after placement if it
hasn't filled, instead of letting it rest until `not_after`. Neither
broker has native per-order expiry, so the worker enforces it via the
existing cron sweep. A resting order gets no further webhooks and the
worker has no session calendar, so Pine computes the forward bar-close
times (`time_close(timeframe.period, bars_back=-N)`, weekend-aware) and
ships them as an unsigned `next_candle_timestamp_1..5` menu; the signed
`expiry_bars` selects a slot, capped at `not_after`.

Status: DONE — all layers landed and green (core, worker, cli, tv-arm).
- `Shell.next_candle_timestamp_1..5` (unsigned) + `Intent.expiry_bars`
  (signed); `resolve_cancel_at`; new `EntryAttempt.cancel_at` (separate
  from `expires_at` on purpose); cron sweep `bar-expiry` branch; CLI
  `TradeSpec.expiry_bars`; `tv-arm --expiry-bars`; Pine v2.3 plots.
- Out-of-range `expiry_bars` → `rejected: expiry-bars-out-of-range` 400,
  no seen-id poison.

Cross-repo coordination: the Pine plots live in
`pine-scripts/candle-signals-v2.pine` (v2.3). The worker falls back to
`not_after` when the menu is absent, and the menu is only emitted into the
signed body when `expiry_bars` is set — so an operator on an older
indicator who doesn't use the flag is unaffected. **Republish the v2.3
study to TradingView before using `expiry_bars` live.**

Follow-up (deferred, not built): `on_broker_rejection` recovery
(skip/market/limit on a `#19-10` reject, with a ≥1R recheck and the
limit-override) — see `BUG-entry-too-close-to-market.md`.

## Active — `prep-expire` action + `<prep>-expiry` vertical line

A vertical line labelled `<prep>-expiry` (e.g. `break-and-close-expiry`,
`retest-expiry`) fires its own alert into the worker carrying a new
`prep-expire` action with `step: <prep>`. The worker records a
`prep-blocked:<account>:<instrument>:<step>` KV flag. From then on, any
`prep` fire for that step is **rejected** (logged, no broker call), so the
entry's `requires_preps` gate for that step can never be satisfied and the
`enter` is rejected too. If the prep already fired *before* the line, it's
already recorded and the trade is legitimately in — the block only stops
*future* preps.

Runtime timeline (the log lines must let us reconstruct this later):

1. `break-and-close-expiry` line fires → worker stores
   `prep-blocked:<acct>:<instr>:break-and-close`. Log: "prep-expire stored".
2. `break-and-close` prep fires → worker rejects (blocked). Log:
   "prep rejected — expired/blocked". Does NOT poison seen-id.
3. `enter` fires → worker rejects (missing required prep). Log:
   "enter rejected — required prep break-and-close not satisfied".

Motivating bug: an H&S break-and-close landed 124 bars after the pattern
start (max 120 on H1) — too late, the trade lost. The expiry line lets the
operator draw the "pattern got too big" cutoff on the chart. (Bar counts
per TF: M15/H1 30–120, H4 30–180, Daily 30–210, Weekly 30–∞.)

### Design decisions (locked with user)

- New `Action::PrepExpire` (wire `prep-expire`), `step: <prep>` required,
  no broker side effects (state only). Marks-seen on completion (idempotent
  control action, like prep/veto).
- Blocked-prep rejection is `ActionResult::Rejected` (logged, no seen-id
  poison) — re-fires are harmless re-logs. Consistent with the 2026-06
  replay-scope fix.
- Label inference: `<name>-expiry` → strip `-expiry`, match `<name>`
  against prep vocabularies (`break-and-close`/`neckline`,
  `retest`/`retrace`) → canonical prep step. `trade-expiry` keeps its
  dedicated whole-trade-close veto meaning (no collision: `trade` ∉ preps).

### tv-arm validation (Part B)

- A `<prep>-expiry` line **in the future** with **no matching prep drawing**
  present → hard **error** (you'd arm a setup that can never enter).
- A `<prep>-expiry` line **in the past** → **warn** only (re-arm later).

### Steps

- [x] conventions: `PREP_EXPIRY_SUFFIX` + `prep_name_from_expiry_label()`
      + `AlertBasename::PrepExpire(step)` (`08-prep-expire-<step>`). Tests.
- [x] core: `Action::PrepExpire`; `Intent::validate` (needs step + ttl;
      `MissingPrepExpireStep`); `StateStore` block-prep methods
      (`block_prep`/`is_prep_blocked`/`clear_prep_block`) +
      KV/memstore/retry-mock impls; `PrepBlockEntry` + `Snapshot.prep_blocks`
      surfacing; `PREP_BLOCK_INDEX_CAP`. Tests (validate + round-trip +
      scoping + snapshot yaml).
- [x] worker: `handle_prep_expire` dispatch + "prep-expire stored"
      timeline log; `handle_prep` consults `is_prep_blocked` → 409
      "prep-expired" with "prep rejected — expired" log, no seen-poison;
      enter gate's existing `missing-prep` log is step 3. Host + wasm
      build + clippy clean; 109 worker tests pass.
- [x] CLI: `TradeSpec.prep_expiries`; emits one drawing-bound
      `08-prep-expire-<step>` alert per cutoff; rejects unknown / skipped
      names; `prompts.rs` PrepExpire arm. Tests.
- [x] tv-arm: classify `<prep>-expiry` lines → `Roles.prep_expiries`
      (latest-wins per step); `alert_spec` binds the prep-expire alert to
      the drawing; `check_prep_expiries` future-with-no-prep error / past
      warn; `prep_expiry_steps` feeds `cli::TradeSpec`. Tests.
- [x] README (actions list, basename table, drawing-roles table) +
      CHANGELOG `v4`.

Status: DONE — all layers landed and green (conventions, core, worker,
cli, tv-arm). Follow-up idea: auto-draw the `<prep>-expiry` line at
`pattern_start + max_bars × resolution` per timeframe (CHANGELOG v4).

## Done — `--require-confirmation` flag on tv-arm

`needs_confirmed` was first-class on `Intent` and on the close path
(`needs_confirmed_close`) but the enter path only had `--require-golden`.
Added the symmetric entry-side gate end to end:

- [x] `TradeSpec.needs_confirmed: bool` (entry-side, symmetric with
      `needs_golden`, distinct from `needs_confirmed_close`).
- [x] Threaded through `build_enter_alert` → `intent.needs_confirmed`.
- [x] `tv-arm --require-confirmation` flag → `spec.needs_confirmed`.
- [x] Tests: enter-only threading, golden+confirmed coexist, arg parsing.
- [x] README tv-arm flag list updated; `cargo test`/`clippy`/`fmt` green.

## Active — fix `too-low` / pcl-exhausted veto closing open positions

Bug: the pcl-exhausted veto (`too-low` for shorts, `too-high` for longs) is
emitted with `level: ClosePositions`. It is an *entry-gate* condition ("price
already ran most of the way to TP, don't open a late entry"), not a thesis
invalidation, so it must never close an open position. Real incident: demo
trade 046 (CHF/JPY H&S short) closed ~31 ticks before TP, costing ~1.29R.
See `BUG-too-low-closes-positions.md`.

Root cause: `build_invalidation_alert()` (`cli/src/trade_patterns.rs`)
hard-codes `ClosePositions` and is reused for *both* the invalidation veto
(correct: close) and the pcl-exhausted veto (wrong: should be entry-block only).

Steps:

- [x] Give `build_invalidation_alert` a `level: VetoLevel` parameter; purpose
      string reflects the level.
- [x] Invalidation veto call site → `ClosePositions` (unchanged behaviour).
- [x] pcl-exhausted veto call site → `StopNextEntry` (the fix).
- [x] Fix the misleading `pcl_exhausted_veto_name` doc-comment (the two vetos
      are *not* the same level).
- [x] Rework existing `..._pcl_exhausted_veto_matches_invalidation_shape` test
      (renamed `..._shares_shape_but_not_level`) — now asserts levels *differ*.
- [x] New regression test `pcl_exhausted_veto_never_closes_positions_for_both_patterns`:
      pcl-exhausted = `StopNextEntry`, invalidation = `ClosePositions`, HS + IHS.
- [x] Audit other `ClosePositions` vetos (`trade-expiry`). Verdict below.
- [x] `cargo test` / `cargo clippy` / `cargo fmt`; README rows 81-82 synced.

Audit verdict (other `ClosePositions` vetos):
- `trade-expiry` — fires at wall-clock expiry (`not_before = trade_expiry`),
  meaning "the setup's planned window is over". Not a price-relative trigger,
  so it can't spuriously fire in the trade's favour. Flattening a stale trade
  past its window is the intended belt-and-braces. **Correct, leave as-is.**
- `invalidation` (`too-high` short / `too-low` long) — fires when price runs
  back past the right shoulder, i.e. *against* the trade, structure broken.
  Genuine thesis invalidation. **Correct, leave as-is.**

## Active — Consolidated close-on-reversal + first-class candle-quality gates

**Bug observed (2026-06-03):** GBP/NZD demo entry SL didn't match any obvious
swing structure, and a closer audit of the close-on-reversal path showed two
deeper issues:

1. The Pine `Long Pattern` / `Short Pattern` plots (used as the
   close-on-reversal trigger) fire on **any** opposite-direction signal,
   golden or not — there is no operator-facing way to require "golden only"
   on a Close, because `Intent::validate` rejects `needs_golden: true` on
   any action ≠ `Enter` (`core/src/intent.rs:699`).
2. We have two separate alert basenames (`06-close-on-reversal`,
   `07-close-on-sr-reversal`) for what is semantically one operation
   ("close when a reversal candle prints inside a meaningful contextual
   window"). The two-alert split is artificial and produces awkward CLI
   plumbing.

### Design

Single `06-close-on-reversal` alert that fires when ALL of:

1. **Inside a configured window** — at least one of:
   - active news window for this `trade_id`, OR
   - current broker price inside a configured price band.
2. **Candle quality** — golden (default), or operator-overridden to
   confirmed-but-not-golden.
3. **(optional) `allow_close` script** — ad-hoc filter, symmetric with
   `allow_entry`.

Plus: promote `require_confirmation` from a script-only check on the
`allow_entry` body to a first-class `needs_confirmed: bool` field on
`Intent`, applicable to both Enter and Close. Symmetric with the existing
`needs_golden`.

### YAML shape (new form)

```yaml
v: 1
action: close
id: <trade_id>-close-on-reversal
trade_id: <trade_id>
instrument: ...
broker: ...
account: ...

# New consolidated gate (replaces require_news_window + require_price_in_ranges)
inside_window: [news, price]              # OR-composed; at least one must be set
sr_bands: [[1.0950, 1.0970]]              # required when "price" is in inside_window

# Candle quality (default: golden only). Mutually exclusive.
needs_golden: true                        # default for reversal closes
# needs_confirmed: true                   # operator opt-in to "confirmed, not necessarily golden"

# Optional ad-hoc filter (symmetric with allow_entry)
# allow_close: |
#   <script expr>
```

### Field naming

- `inside_window: [news, price]` — list of window types under which the close
  is valid. List-implies-any (OR), same surface area as `requires_preps` but
  with opposite composition. Documented explicitly in the field doc-comment
  and the README. The two-axis metaphor: news is a time-window, price is a
  price-window.
- `sr_bands: Vec<[f64; 2]>` — the data for the "price" window type.
  Required when `price` ∈ `inside_window`; rejected when it's not.
- `needs_confirmed: bool` — symmetric with existing `needs_golden`. Both
  rejected on actions ≠ Enter|Close.
- `allow_close: Tunable<bool>` — symmetric with existing `allow_entry`.

### Wire-compat / deprecation

- Old fields `require_news_window` and `require_price_in_ranges` stay
  working unchanged (the worker already OR-composes them via
  `evaluate_close_gates` at `src/lib.rs:596` — verified). They are marked
  deprecated in doc-comments. Old in-flight alerts continue to fire
  correctly.
- An intent cannot mix old and new forms; validate-time rejection.
- `07-close-on-sr-reversal` basename stays in the `AlertBasename` enum for
  inbound decode of in-flight alerts; CLI stops emitting it.

### Steps

- [x] **Step 1: worker — validation relaxation + new fields.**
  - Relax `Intent::validate` to allow `needs_golden: true` on
    `Action::Close` (currently rejected at `core/src/intent.rs:699`).
  - Add `needs_confirmed: bool` to `Intent`. Same shape as `needs_golden`.
    Validate-time: only valid on Enter|Close.
  - Add `inside_window: Vec<EventWindow>` and `sr_bands: Vec<[f64;2]>`.
    `EventWindow` is an enum `News | Price` with kebab serde.
  - Validate-time on Close: if either of the new fields is set, the old
    fields must be empty (mutual exclusion). At least one window-type
    gate must resolve to a real check.
  - Tests: round-trip, mutual-exclusion rejection, missing-data rejection
    (`price` in `inside_window` without `sr_bands`).
  - No worker dispatch changes yet — just types.
- [x] **Step 2: worker — Close dispatch consumes new fields + candle gates.**
  - Extend `run_close` (`src/lib.rs:480`) to evaluate the new
    `inside_window` field via the same `GateOutcome` machinery. New form
    routes to the same `evaluate_close_gates` outcome that the old form
    already uses (so the OR semantics are guaranteed identical).
  - Add `needs_golden` / `needs_confirmed` shell-check before the broker
    call. Extract the existing `needs_golden` check from
    `src/allow_entry_gate.rs:51` into a shared helper used by both
    Enter and Close paths.
  - Add `allow_close` script evaluation symmetric with `allow_entry`.
  - Tests: golden-only blocks confirmed-non-golden, news+price OR, both
    failing rejects, allow_close composes AND with the rest.
- [x] **Step 3: CLI — consolidate `06`/`07` builders.**
  - `build_close_on_reversal_alert` becomes the sole reversal-close
    builder. Accepts `inside_window` + `sr_bands` derived from the
    `TradeSpec` `close_on_news` + `sr_reversal_ranges` deprecated input
    fields. `TradeSpec.needs_confirmed_close: bool` flips the candle
    gate from `needs_golden: true` (default) to `needs_confirmed: true`.
  - Deleted `build_close_on_sr_reversal_alert`. CLI no longer emits the
    `07-close-on-sr-reversal` basename (the enum variant stays for
    inbound decode of in-flight alerts; see step 2's wire compat note).
  - Test rewrites: the `06`/`07` split tests became one-alert tests,
    plus a new `needs_confirmed_close` test. 209 cli tests pass.
- [x] **Step 4: Python emitter — obsoleted via deprecation.**
  - The chart-arming frontend has already been ported from
    `scripts/tv_arm_hs.py` to the Rust `tv-arm` crate. The Python
    script hasn't been behaviourally touched since 2026-05-29
    (`7034cef add 07-close-on-sr-reversal`); subsequent work has all
    landed in `tv-arm/`.
  - `tv-arm/src/pipeline.rs::build_trade_spec` still populates the
    same input-side fields (`close_on_news`, `sr_reversal_ranges`)
    on `cli::TradeSpec`. Step 3's consolidated
    `build_close_on_reversal_alert` then routes those into
    `inside_window` + `sr_bands` on the emitted intent — so the
    Rust path produces the new wire form transparently with no
    further changes.
  - Marked `scripts/tv_arm_hs.py` deprecated: module docstring
    banner, runtime stderr warning at top of `main()`, argparse
    description tag. Script still runs if invoked.
  - Memory updated: `tv_arm_rust_supersedes_python` flags this for
    future sessions.
- [x] **Step 5: README + per-commit doc sync.**
  - `close` action documented with the three gate layers:
    contextual-window (`inside_window` + `sr_bands`, OR-composed),
    candle-quality (`needs_golden` / `needs_confirmed`, AND-composed),
    ad-hoc filter (`allow_close` Rhai script).
  - Deprecated-form note on `require_news_window` /
    `require_price_in_ranges` (still accepted for in-flight alerts).
  - Alert-basename table collapsed from 06+07 split to a single
    consolidated `06-close-on-reversal` row.
  - Chart-arming section renamed from `scripts/tv_arm_hs.py` to
    `tv-arm`; CLI example switched to `cargo run -p tv-arm --`.
    Deprecation callout points operators away from the Python script.
  - News-pair / S-R-line drawing-table rows updated to describe the
    consolidated alert behaviour.

### Open follow-ups (not blocking the bug fix)

- Investigate the **6-7 min lag** between H1 close and broker fill on the
  2026-06-03 demo entries — see `entry_fill_lag_after_h1_close` memory.
  Most likely TV alert-eval lag; confirm via Cloudflare log timestamps.
- Audit the **GBP/NZD long SL drawing** (2.27736) — looks like it doesn't
  match the right-shoulder structure on chart, possibly a fib-anchor
  drag error on the operator side, not a code bug. Re-check after
  next setup is armed.

## Done — encryption retired; HMAC signing is the only auth

The encrypted envelope path (ChaCha20-Poly1305 over a `v1.<base64>`
payload) has been removed. Auth is now HMAC-SHA256 only, over the
cleartext body, via the existing `core::sig` module and the
`sign` / `verify` CLI verbs.

What changed:

- `core::crypto` module deleted. `parse_key_hex` / `KEY_LEN` moved to
  `core::sig` since the byte format is shared.
- `Shell.payload` removed.
- `IncomingError::Decrypt` removed; `parse_and_verify` no longer
  branches by envelope shape — every body is signed.
- `Cmd::Encrypt`, `Cmd::Decrypt`, `EncryptArgs`, `DecryptArgs`,
  `run_decrypt`, `extract_payload_blob`, `wrap_in_envelope`,
  `build_yaml_template`, `build_yaml_control_body`, `encrypt_intent`
  all deleted from the CLI. `EndpointArgs.signed` flag gone (always
  signed).
- `ENCRYPTION_KEY` worker secret renamed to `SIGNING_KEY`. The diag
  routes' `X-Diag-Key` header now compares against `SIGNING_KEY` too.
- `chacha20poly1305` dropped from `core/Cargo.toml`.

Migration for the deployed worker:

1. `wrangler secret put SIGNING_KEY < ~/.config/trade-control/key.hex`
   (same bytes as the old `ENCRYPTION_KEY`).
2. Deploy.
3. `wrangler secret delete ENCRYPTION_KEY`.

TradingView alert bodies that still use the encrypted format (top-level
`payload: "v1.…"`) will fail with a sig error after deploy — regenerate
them via `trade-control sign`. The user confirmed no live alerts in
the pipeline carry the old format.

269 tests pass after the cut (16 broker-oanda + 81 cli + 158 core +
14 worker); clippy + fmt clean on host + wasm.

## Done — TradeNation session lifecycle (wasm-side login)

The worker re-authenticates itself via per-account credentials stored
in `TN_ACCOUNT_<NAME>` secrets — no external rotation needed. On
cached-session rejection, the next request transparently re-logs in
using the stored credentials and writes the new session to KV. Both
demo and live login paths run inside the wasm worker via the
`worker::Fetch` crate (`reqwest`'s wasm shim auto-follows redirects
and can't be used).

The pre-named-accounts cron shim (`scripts/refresh-tn-session.sh`,
`TN_SESSION_JSON` secret, `TN_DEMO_LOGIN_ID` / `TN_DEMO_PASSWORD`
globals) was retired alongside Step 5 below. If you have stale
`TN_SESSION_JSON` / `TN_DEMO_*` secrets in the Cloudflare deployment,
run `wrangler secret delete` for each — the worker doesn't read them
anymore.

## Active — First-class accounts

Lift account selection out of "one TN session per worker" and into
named records, so a single deploy can route different intents to
different broker accounts (demo / live, OANDA / TradeNation).

Security model: metadata in KV (no secrets), credentials in
Cloudflare Secret Store (one binding per account). KV-only exfil
yields no password material. See conversation log 2026-05-19 for
the design rationale.

Steps:

- [x] **Step 1: core account types & traits.** `core::account` module
      adds `AccountKind`, `AccountCaps`, `AccountMetadata`,
      `Credentials` (TradeNation/OANDA variants), `MetadataStore`,
      `CredentialsResolver`, and the bundled `AccountStore`. In-memory
      impls for tests. 39 new tests, no worker integration yet.
- [x] **Step 2a: admin routes.** Worker-side `KvMetadataStore` +
      `SecretCredentialsResolver` (wasm) + four routes under `/admin/`
      gated by an `X-Admin-Key` header backed by a new `ADMIN_KEY`
      secret (distinct from `ENCRYPTION_KEY`). Routes:
      - `GET    /admin/accounts`           — list as YAML
      - `POST   /admin/accounts`           — add (JSON body)
      - `DELETE /admin/accounts/<name>`    — remove from index
      - `POST   /admin/accounts/<name>/test` — verify metadata +
        credential secret + broker match (no broker login yet)
      `wrangler secret put ADMIN_KEY` required before deploying. The
      credential secrets follow the schema `TN_ACCOUNT_<NAME>` /
      `OANDA_ACCOUNT_<NAME>` (name uppercased, `-`→`_`); blob is
      the JSON serialisation of `core::account::Credentials`.
- [x] **Step 2b: CLI verbs.** `encrypt-payload account
      list / add / delete / test` subcommands wired through the admin
      routes. Auth is `--admin-key-file` (env
      `TRADE_CONTROL_ADMIN_KEY_FILE`), separate from `--key-file`. `add`
      prompts for credentials via `dialoguer::Password` and pipes the
      JSON to `wrangler secret put` over stdin (no argv leakage); use
      `--no-secret` to skip the wrangler step. `delete --purge-secret`
      also runs `wrangler secret delete` (requires `--broker` so the
      binding name can be computed locally). New CLI modules:
      `admin_client.rs` (HTTP) and `admin_secret.rs` (wrangler shell-out).
      77 cli + 5 cli-bin tests pass; clippy + fmt clean on host.
- [x] **Step 3: plumb `account:` into the intent.** `Intent` gains an
      optional `account: Option<String>` field
      (`skip_serializing_if = Option::is_none` for back-compat).
      `acquire_tn_broker` now takes `account: Option<&str>`; when set,
      it routes through `KvMetadataStore` + `SecretCredentialsResolver`,
      caches the session under `tn:session:<name>` (per-account, so
      multiple TN accounts don't fight over one slot), and uses the
      account's own credentials. Demo accounts use the existing
      redirect-chain login with the account's username/password. Live
      accounts return `None` with a clear log (step 4 wires the live
      login). Account-less intents keep the legacy path
      (TN_SESSION_JSON / TN_DEMO_LOGIN_ID). `/diag/fx` and
      `/diag/candles` accept an optional `?account=…` query param.
      CLI `encrypt`: `account` is now an optional prompted field on
      enter/close/invalidate/veto (the broker-touching actions); blank
      input skips it so the wire form stays minimal. Worker
      `/admin/accounts/.../test` now emits the lowercase wire-form
      `broker:` / `kind:` values to match `list`. 149 worker + 74 cli
      tests pass; clippy clean on host + wasm.
- [x] **Step 4: live login path** (`login_live` in `tn_login.rs`).
      Drives the JWT → auth0 → cloudtrade hops, then reuses the
      existing redirect-chain harvest on the cloudtrade one-time URL.
      Three new helpers, all wasm-side: `get_jwt` (POST
      `tradenation.com/signup/api/login` with JSON body), `pick_account_id_from_jwt`
      (GET `portal.cube.finsatechnology.com/auth0/user` with Bearer),
      and `get_platform_url` (POST `…/cloudtrade/login` with Bearer +
      `account_id`). The platform-bootstrap step rejects sessions with
      no OTS — live writes use the OTS as the request `key`, so a
      missing OTS would silently break trade time; better to refuse
      here. Account-picking logic (`pick_funded_account`) is factored
      into `tn_login_helpers.rs` so it's host-testable (the wasm-only
      `tn_login` module isn't reachable under `cargo test`); also a
      shared `truncate_for_log` for trimming TN error bodies in logs.
      Wired into `acquire_tn_broker_for_account` via a new
      `login_and_cache_live` helper that mirrors `login_and_cache_demo`;
      the cache/serialise tail is now a single `cache_and_open` to
      avoid duplication. 14 worker + 149 core + 74 cli tests pass;
      clippy + fmt clean on host + wasm.
- [x] **Step 5: retire legacy fallback.** Removed
      `acquire_tn_broker_legacy` and its three constants
      (`TN_SESSION_JSON`, `TN_DEMO_LOGIN_ID`, `TN_DEMO_PASSWORD`,
      `TN_SESSION_KV_KEY`). TN routing now requires an `account:` on
      the intent — without one, the worker returns 503 with a clear
      "missing account" error. `scripts/refresh-tn-session.sh`
      deleted; the named-account path auto-relogs on cached-session
      rejection so no external rotation is needed. README updated to
      reflect the new TN session story.
- [x] **Three-way sizing modes + dry-run on intent.** `Intent` gains
      three new optional fields, mutually exclusive with each other
      and `risk_pct`:
      - `risk_amount: Option<f64>` — fixed money risk per trade in
        account currency (e.g. `1.0` to "bet $1").
      - `size_units: Option<f64>` — literal position size (e.g.
        `0.01` for one micro-lot). Bypasses sizing math entirely.
      - `dry_run: Option<bool>` — resolve + log sizing inputs/output,
        skip broker call. Useful for verifying templates safely on
        a live account.
      `Resolved` carries a new `RiskBudget` enum
      (`Percent(f64)` / `Amount(f64)` / `Units(f64)`). Resolver
      rejects multi-set or invalid values at the edge.
      OANDA broker consumes all three modes: `Percent` and `Amount`
      go through `units_for_budget`; `Units` skips sizing but still
      enforces `MAX_RISK_PCT_PER_TRADE` by reconstructing the
      implied money risk (`units * stop_distance` ÷ equity). TN
      adapter rejects both `Amount` and `Units` for now with clear
      logs (upstream `broker-tradenation` still takes `risk_pct`
      only — bumping it is a separate pass). Dry-run short-circuits
      in the worker dispatch before `place_entry` and logs id /
      instrument / direction / entry / SL / TP / risk-mode /
      implicit R; works for both brokers. 279 tests pass (77 cli,
      167 core, 16 broker-oanda, 14 worker, 5 cli-bin); clippy +
      fmt clean on host + wasm.
- [x] **Step 6: extend `AccountCaps` with `min_position_size`.**
      Done. New optional `min_position_size: Option<f64>` field on
      `AccountCaps`, surfaced via `--min-position-size` on `account
      add`. Worker loads `meta.caps.min_position_size` from
      `KvMetadataStore` and threads it through `EntryRequest`. Both
      brokers (TN adapter + OANDA `place_entry`) enforce the floor
      against explicit `RiskBudget::Units(s)` — `s < min` returns
      `UnitsBelowMinimum` before the broker is called. `Percent` /
      `Amount` modes skip the client floor because they compute
      units after equity/FX lookup; the broker's own
      `UnitsBelowMinimum` covers them. 276 tests pass (16
      broker-oanda + 81 cli + 159 core + 20 worker); clippy + fmt
      clean on host + wasm.

## Done

- **`fx_rate` rewrite to use live chart prices** — landed and deployed
  as `broker-tradenation-v0.3.0`. The root cause of the `risk_amount
  must be positive and finite, got 0` sizing failures was that
  TradeNation's `GetMarketQuote` returns `Bid: 0` and `Ask: 0` for
  every market — live prices were originally pushed over a WebSocket
  which has been silent since 2026-04-27 (only sends a `connectResponse`
  frame, then rejects every envelope with `Invalid request`). The v0.2.0
  zero-guard made the failure visible but couldn't fix it.

  The fix: `fx_rate` now resolves the pair to a `market_id` via
  `resolve_market` (unchanged) then fetches the latest 1-minute bid
  and ask candles from the unauthenticated
  `charts.finsatechnology.com/data/minute/{market_id}/{bid|ask}?l=1`
  endpoint, computing `mid = (bid_close + ask_close) / 2`. The chart
  endpoint needs no auth — only `Origin: https://chart-cfd.tradenation.com`
  and `Referer` headers — and works fine from inside wasm via
  `reqwest`'s wasm shim. Direct/inverse fallthrough preserved.

  Verified end-to-end via `GET /diag/fx`:
  - GBP/USD: 1.34182 (was 0.0)
  - USD/GBP: 0.7453 (inverse path)
  - EUR/USD: 1.16459
  - GBP/AUD: 1.87822

  Also extended the diag module with `GET /diag/candles?market_id=N&type=bid|ask&tf=minute&count=1`
  which hits the chart endpoint directly via `broker.client()` —
  useful for verifying a single market's chart data without involving
  `fx_rate`'s resolution logic. Worker bumped to
  `broker-tradenation-v0.3.0`, deployed to
  `trade-control-web-hook.msherborne.workers.dev`. 188 worker tests
  pass; wasm + host builds clippy-clean.

- **`GET /diag/fx` endpoint + upstream `fx_rate` zero-guard** —
  landed. New `src/diag.rs` module owns read-only diagnostic routes;
  `GET /diag/fx?from=GBP&to=USD` runs `tradenation_api::fx_rate`
  against the cached TN session and returns YAML with the resolved
  rate (or the error string). Auth via `X-Diag-Key` header whose
  value must equal the `ENCRYPTION_KEY` secret — re-using the
  existing key keeps secret management single-secret. Routing splits
  GET (diag) from POST (the existing encrypted-envelope handler)
  before body parsing.

  Why: TN's `fx_rate` was returning `Ok(0.0)` for `GBP/USD` during
  out-of-session hours, which flowed through to `stake_for_risk` as
  `risk_amount must be positive and finite, got 0` — diagnostic
  obscured behind two layers. The diag endpoint lets the operator
  reproduce the actual `fx_rate` output without firing a real entry.

  Upstream fix shipped as `broker-tradenation-v0.2.0`:
  (a) `fx_rate`'s direct branch now guards against zero mid
  (symmetric to the existing inverse-branch guard) and falls through
  to the inverse pair; if both fail it returns a `TradeError::Decode`
  carrying "direct FX pair X/Y has non-positive mid 0" — exactly what
  the operator needs to see.
  (b) `TradeNationBroker::client()` getter so consumers can call
  `tradenation_api::fx_rate` directly with the same `reqwest::Client`
  the broker uses (cookie / connection state stays consistent).

  Cargo.toml bumped to `broker-tradenation-v0.2.0`. Wasm + host
  builds clippy-clean.
- **`tracing` → `console_log` subscriber in the worker** — landed. New
  `src/tracing_console.rs` implements a minimal `tracing::Subscriber`
  (~110 lines) that formats events as `LEVEL target: field=value …` and
  routes `WARN`/`ERROR` to `worker::console_error!`, everything else to
  `worker::console_log!`. Installed once per worker instance via a
  `OnceLock` at the top of the fetch handler. Why: broker crates
  (notably `broker-tradenation`) log error detail through `tracing::warn!`
  / `tracing::error!`, but without a subscriber installed those events
  are silently dropped in wasm — so the worker's own lossy
  `entry failed: broker rejected the order` was the only breadcrumb. Now
  the actual TN rejection reason shows up in Cloudflare's request log.
  Step 1 of 2; step 2 is propagating the broker error string through
  `EntryError::OrderRejected(String)` once we've seen what TN actually
  says. Clippy clean on host + wasm targets.
- **`clear-prep` also forgets the prep's setter `seen:<id>`** —
  landed. Prep KV values now store `<rfc3339>|<setter_id>` instead of
  bare `<rfc3339>`, so the worker remembers which message-id set each
  prep. `clear_prep` returns the setter id; `handle_clear_prep` and
  `clear_named_preps` (the cascade-clear path triggered by a fresh
  upstream prep's `clears:` list) call a new `forget_seen` method that
  deletes `seen:<id>` and prunes the index. This means the operator
  can re-send the original prep message after `clear-prep` without
  hitting a 409 from replay protection. Wire format is
  forward-compatible — legacy bare-timestamp values still parse
  (empty setter_id, no seen-forget). 106 core + 67 cli + 5 cli-bin
  tests; clippy clean.
- **Per-instrument trade-expiry anchor (CLI-only)** — landed. New
  `cli::expiry` module persists a single `DateTime<Utc>` per instrument
  under `$XDG_CONFIG_HOME/trade-control/expiry/<INSTRUMENT>.txt`. The
  interactive flow asks for the anchor up-front when the operator
  declares a `veto` with `name: trade-expiry`, and stores whatever they
  enter (relative durations like `2d` accepted, ISO-8601 accepted).
  Subsequent prep/veto/enter prompts use the anchor as the default for
  `not_after` (read-only on `enter`), and prep/veto get a derived
  `ttl_hours` default (hours-from-now rounded up). A stale (past)
  anchor is silently dropped on load and the prompts fall back to the
  prior defaults (`8h` / `4`). Pure UX sugar — the worker neither sees
  nor cares about the anchor. Also fixed the save-as-template prompt
  so blank-Enter actually skips (previously the default value field
  meant Enter saved to `new.yaml`). 67 cli lib tests pass; clippy
  clean.
- **HMAC-signed cleartext wire format (parallel to encrypted)** —
  landed. New `core::sig` module: canonical form = fixed `v1-sig` tag,
  sorted schema-fingerprint of top-level keys (CSV), then `key=value`
  lines for every signed field, HMAC-SHA256 with `subtle::ct_eq` for
  verify. Shell fields (`close`/`high`/`low`/`time`) have their keys
  signed but their values excluded — so TradingView's `{{close}}` →
  number substitution doesn't invalidate the sig, but dropping a shell
  key does. Worker detects format by field presence (`sig:` vs
  `payload:`) and both paths run in parallel. CLI gains `--signed` on
  `encrypt`, `status`, `unlock`, `prep`, `veto`, `clear-prep`,
  `clear-veto`, plus a `verify` subcommand (mirror of `decrypt` for the
  signed path). Why: cleartext bodies show up in Cloudflare's request
  log so operators can read what TradingView sent without round-tripping
  through `decrypt`. Auth is unchanged (32-byte key, same key file).
  103 core + 55 cli + 5 cli-bin tests pass; clippy clean. End-to-end
  round-trip verified: signed encrypt → simulated TV substitution →
  verify, plus tamper and wrong-key rejection.
- **`decrypt` subcommand + clap-complete shell completions** — landed.
  `encrypt-payload decrypt --key-file KEY [BLOB]` accepts either a bare
  `v1.<base64>` blob as a positional, the full YAML alert body on stdin,
  or a `--file PATH`. Tolerates TradingView `{{placeholder}}` shells by
  scanning lines for `payload:` rather than parsing as YAML, so a body
  pasted straight from the alert template still decrypts. Plus
  `encrypt-payload completions <shell>` prints a clap-generated
  completion script — install with
  `encrypt-payload completions zsh > ~/.zfunc/_encrypt-payload`. 5 new
  tests for the payload extractor; round-trip with a minted blob
  verified.
- **Prep `clears` list to fix stale-ordering bug** — landed. `Intent`
  gains a `clears: Vec<String>` field. The `Prep` handler clears each
  listed prep step before recording the new one; the `Veto` handler
  does the same for vetos (symmetry). Fixes the bug where a stale
  `retest` from before `break-and-close` was satisfied stuck around
  forever and falsely satisfied future ordered gates. CLI gains
  `--clears foo,bar` on the `prep` and `veto` subcommands; the encrypt
  flow prompts for `clears` on prep/veto actions. Recent prep / veto
  names are fuzzy-picked from history (typo-proof) for both the
  `step`/`name` field and the list-of-names prompts. 84 core tests +
  55 cli tests pass; clippy clean.
- **YAML wire format + interactive template-driven encoder** — landed. Interactive prompts wired through `dialoguer` (gated behind the `cli` feature).
- **Queryable state endpoint + CLI state-management client** — landed.
  `status` and `unlock` actions go through the same encrypted envelope as
  enter/close/invalidate. KV maintains `index:cooldowns` and `index:seen`
  JSON arrays alongside the TTL keys so `status` can list them. CLI gains
  `status` and `unlock <INSTRUMENT>` subcommands that POST to the deployed
  worker via reqwest::blocking. 68 lib tests pass.

## Phase 2 ideas (parked — captured for later, not building yet)

### Multi-stage trendline workflow

Instead of one single alert fires-and-enters, a setup is built up by a *chain* of TradingView alerts, each advancing a state machine inside the webhook:

1. **Break-and-close alert** — TradingView fires when price breaks and closes through a hand-drawn trendline. Worker records `setup:<id>` in pending state.
2. **Retest alert** — fires when price retraces back to the trendline. Worker advances the setup to "armed".
3. **Entry-candle alert** — fires on the next candle's signal. Worker only places the order if the setup id is `armed` and not invalidated.
4. **Pre-fill SL-hit alert** — a separate alert at the planned stop-loss price. If this fires before the entry-candle alert, the setup id is locked out for 12h (and any pending order cancelled).

Implications for the encrypted intent format:
- Add a `setup_id` field separate from the per-message `id` — the chain shares one `setup_id`, each alert has a unique `id` for replay protection.
- Add an `expected_state` field per alert: `expect: break_close | retest | entry | invalidate_at_sl`. Worker transitions the state machine instead of blindly placing an order.
- `StateStore` needs a `get_setup_state(setup_id)` / `set_setup_state(setup_id, state, ttl)` pair on top of the current `seen` / `cooldown` pair.

This is significantly more state than what the MVP carries; design it after the simple pin-bar flow is proven in live use.

When this lands, the CLI gains a third subcommand `list-setups` — show all setup state machines and their current state. Reuses the existing `status` / `unlock` plumbing.

### Carried-over blocker

`cargo build --target wasm32-unknown-unknown --lib` currently fails inside
`oanda-client` with a `BidAskDataSource: Send` regression — pre-existing,
not introduced by anything in this repo. Needs an upstream fix in
oanda-client before `wrangler deploy` will work again.
