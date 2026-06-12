# Changelog

## v13 — 2026-06-12 — experimental `veto_on_reversal` on reversal-close

### Why

A real setup got its `break-and-close` and `retest` preps, then price
reversed off a support line **before** the entry fired, and the trade
entered anyway and lost. The reversal-close machinery only flattens an
*open* position — fired before entry it's a no-op, so the entry sailed
through despite a strong "this trade won't work" signal. We want the same
reversal that would close the trade to optionally *veto the upcoming
entry* when it lands pre-entry.

### What changed

- New **opt-in, default-off** field `Intent.veto_on_reversal: bool`. On a
  price-windowed `close` (the reversal-close), when the close gate passes
  the worker also writes a `reversal` veto scoped to the intent's
  `trade_id`. A later `enter` for that setup then hits the existing
  `is_vetoed` gate and is rejected.
- Semantics are **StopNextEntry-style**: the veto only blocks future
  entries; it never force-closes a position beyond the close the intent
  already performs (consistent with "entry-gate vetos must not close
  positions"). Written on **every** gate-pass — pre-entry it blocks the
  entry; post-entry it harmlessly prevents a re-entry for the rest of the
  window. TTL = life of the alert window (`veto_ttl_seconds`).
- The worker reuses the existing `set_veto` / `is_vetoed` machinery — no
  new state primitive. The veto name is the fixed string `reversal`.
- CLI: `TradeSpec.veto_on_reversal` plumbs the flag onto the emitted
  `06-close-on-reversal` intent, but only when `sr_bands` are present (a
  news-only reversal-close has no band to reverse off).
- tv-arm: new `--veto-on-reversal` flag (default off) sets it at arm time.

### Breaking

None. The field default-skips on serialize, so existing alerts are
byte-identical and in-flight bundles are unaffected.

### Config

- Intent wire: `veto_on_reversal: true` (optional, only on a
  price-windowed `close`).
- CLI spec: `veto_on_reversal: true` in `trade.yaml`.
- tv-arm: `--veto-on-reversal`.

### Validation

`veto_on_reversal` is rejected on a non-`close` action
(`VetoOnReversalOnNonClose`) and on a `close` with no price window
(`VetoOnReversalWithoutPriceWindow`).

### Tests

- core: default-off skip-serialize, round-trip when set, accepts the
  deprecated `require_price_in_ranges` price window, rejects on non-close,
  rejects without a price window.
- cli: flag rides onto the emitted close when armed + bands present, stays
  off by default, and is suppressed for a news-only reversal-close.
- worker: `reversal_veto_plan` scoping (trade_id / account / instrument),
  None without a `trade_id`, and TTL spanning to the window end.

### Follow-up

Experimental — promote past default-off only after a demo run shows it
blocks losers without killing legitimate post-stop re-entries on
multi-shot setups.

## v12 — 2026-06-12 — align remaining workspace crates to broker-tradenation-v0.8.0

### Why

v11 bumped the worker lib's `tradenation-api` pin but missed two other
workspace members that depend on the same git repo: `cli/` (the
`trade-control` CLI) and `tv-arm/`. Both still pinned the old source —
`cli` via `branch = "main"` + `version = "0.1.0"`, `tv-arm` via
`tag = "broker-tradenation-v0.7.0"`. With the lib now resolving the repo to
0.2.0 (`v0.8.0`), `deploy.sh`'s `cargo install --path ./cli` step failed:

```
failed to select a version for the requirement `tradenation-api = "^0.1.0"`
candidate versions found which didn't match: 0.2.0
```

A git dependency unifies to one source per repo across a workspace, so the
mismatched pins also forced Cargo to compile the repo **twice** (v0.7.0 +
v0.8.0 trees side by side).

### What changed

- `cli/Cargo.toml`: `tradenation-api` and `tradenation-instrument-cache`
  moved from `branch = "main"` / `0.1.0` to `tag = "broker-tradenation-v0.8.0"`
  / `0.2.0`.
- `tv-arm/Cargo.toml`: `tradenation-api` moved from `v0.7.0` / `0.1.0` to
  `v0.8.0` / `0.2.0`.
- Neither crate touches the renamed timestamp record fields — `cli` uses the
  client/order/instrument-cache APIs, `tv-arm` only `TradeNationClient` +
  `latest_bid_ask` (in a test). No code changes needed; both compile clean.
- `Cargo.lock` drops the entire duplicate v0.7.0 subtree (−93 lines); the
  workspace now has a single `tradenation-api` source.

### Verification

`cargo install --path ./cli` (the failing deploy step) now succeeds.
Whole-workspace `build --all-targets`, `test` (375 + 112 + 139 + 76 + 23
…), `clippy -D warnings`, `fmt --check`, and the wasm32 lib build all pass.

## v11 — 2026-06-12 — bump tradenation-api to broker-tradenation-v0.8.0

### Why

Upstream `tradenation-api` shipped `broker-tradenation-v0.8.0`
(tradenation-api 0.2.0 / broker-tradenation 0.8.0), which converts all
broker timestamps from London-local to Brisbane (UTC+10) inside the crate
and renames six record fields: the base name now holds the converted
`Option<DateTime<FixedOffset>>` and a new `*_original` sibling keeps the
raw broker string.

### What changed

- Both `broker-tradenation` and `tradenation-api` pins moved from
  `tag = "broker-tradenation-v0.7.0"` to `v0.8.0`.
- Only the **test helpers** in `src/tradenation_adapter.rs` touched the
  renamed fields: `opening_order()`, `position()`, and `closed_trade()`
  now build `period`/`creation_time`/`transaction_date`/`open_period` as
  `None` and set the matching `*_original` to `String::new()`.
- The production matching logic (order-id / ref-id correlation in
  `compute_attempt_state`) reads none of the renamed timestamp fields, so
  it is unchanged. No worker-visible behaviour, wire-format, action, CLI,
  gate, secret, or drawing change — README untouched.

### Breaking

None for this crate's API. The dependency's record structs changed shape
(see upstream), but the worker only constructs them in tests.

### Tests

Existing 112-test suite passes unchanged; wasm32 build verified.


### Why

A `too-high` / `too-low` veto set during one setup could block a later,
unrelated entry on the same instrument. The veto KV key was
`veto:<account>:<instrument>:<name>` — no `trade_id` — and the veto's TTL
is stretched to outlive the setup that set it (`veto_ttl_seconds` extends
to the alert's `not_after` plus a tail). A setup with a multi-day
`not_after` therefore left a live veto key sitting in KV for days, and the
operator's next entry on that pair was silently rejected (HTTP 412
`veto-active`) against a veto they'd forgotten existed. Reported
2026-06-11: a missed trade, the blocking veto set "a long time ago" and
invisible in the recent logs.

### What changed

The veto key now carries the setup id:
`veto:<account>:<trade_id>:<instrument>:<name>`. A veto recorded under one
`trade_id` only blocks entries that carry the **same** `trade_id`; a
veto from a different setup on the same instrument no longer matches. The
`enter` gate looks vetos up by the entry's own `trade_id` (every alert in
a `build-trade` bundle already shares one minted id, so the veto and the
entry it guards agree).

`trade_id` is now **required** on `enter`, `veto`, and `clear-veto` —
`Intent::validate` rejects an intent that omits it
(`IntentValidationError::MissingTradeId`, surfaced as HTTP 400). This is a
hard fail by design (operator decision): every trade needs an id, no
instrument-wide fallback. `MissingTradeId` is checked before the older
`MaxRetriesWithoutTradeId` / `MissingTtlHours` checks, so an untagged
enter/veto now reports the missing id first.

### Breaking

- `StateStore::set_veto` / `is_vetoed` / `clear_veto` gain a `trade_id:
  &str` parameter (after `account`). All impls (KV, in-memory, mocks)
  updated.
- `core::state::clear_named_vetos` gains a `trade_id: &str` parameter.
- `core::state::VetoEntry` gains a `trade_id: String` field (surfaced in
  the `status` snapshot under each `vetos:` entry).
- `cli::build_veto_intent` / `build_clear_veto_intent` gain a `trade_id:
  &str` parameter.

### Config / CLI

- `trade-control veto` and `trade-control clear-veto` gain a required
  `--trade-id <slug>` flag.
- The interactive `sign`/`encrypt` questionnaire now prompts for
  `trade_id` on `veto` / `clear-veto`.

### KV migration

Old `veto:<account>:<instrument>:<name>` keys in the deployed KV are no
longer read (lookups use the new trade_id-bearing key) and TTL out on
their own — no wipe required. Any veto an operator wants gone *now* can be
read back from `trade-control status` (the `vetos:` block lists each
`trade_id`) and cleared with `clear-veto --trade-id`.

### Tests

- core: `validate_rejects_enter_without_trade_id`,
  `validate_rejects_veto_without_trade_id`,
  `validate_rejects_clear_veto_without_trade_id`,
  `validate_accepts_veto_with_trade_id`;
  `memstore_veto_scoped_per_trade_id` (veto under trade A does not block
  trade B on the same instrument + account). Existing enter/veto validate
  tests updated to carry a `trade_id`.
- cli: `veto_intent_round_trips` / `clear_veto_intent_carries_name` now
  assert the `trade_id` is set and the built intent validates.

All green: core 375, worker 112, cli 230 + 8; clippy + fmt clean on host
+ wasm.

## v9 — 2026-06-10 — calendar-bars resolves instruments via instrument-lookup

### Why

`calendar_bars::parse_instrument` resolved the trade's instrument through
the legacy `trade_calendar_maker::Instrument::from_oanda_symbol`, which
only understands OANDA forex-style symbols (`EURUSD` after stripping
`_`/`/`). TradeNation index and spread/diff MarketNames — e.g.
`Wall St 30 / Germany 40 Rolling Future Diff` (chart symbol `US30DE40`) —
failed with `unsupported instrument symbol`, so the `calendar-bars` step
was silently skipped (caught as a WARN) during a `tv-arm` run, producing
no auto pause/news bars for that setup.

### What changed

`parse_instrument(raw, broker)` now resolves through the canonical
`instrument-lookup` catalog: by the broker's own symbol first (the form
the caller passes; `broker` is carried on `CalendarBarsArgs`), then a
broker-agnostic `resolve` for canonical ids / cross-broker symbols. The
`Instrument` is built from `asset.news_currencies` (→ `affected_currencies`,
the only field consumed downstream via `is_affected_by`) and `asset.class`
(→ `InstrumentType`; `Crypto`/`Stock` fold into `Index`). Retires one of
the partial instrument maps flagged for migration in `CLAUDE.md`.

### Breaking

- `cli::parse_instrument` gains a second argument:
  `parse_instrument(raw: &str, broker: BrokerKind)`. There is **no** legacy
  `from_oanda_symbol` fallback — an instrument the catalog doesn't know is
  now a hard error pointing the operator at
  `~/.config/instrument-lookup/mappings.toml`, instead of silently
  mis-deriving news currencies from a string heuristic.

### Config

- New `instrument-lookup` path dependency on the `cli` crate. Instruments
  not in the baked-in catalog (e.g. TradeNation diff/spread CFDs) need an
  `[[asset]]` overlay entry in `~/.config/instrument-lookup/mappings.toml`.

### Tests

- `parse_instrument` tests rewritten for the new signature and catalog
  backing: OANDA `EUR_USD`, TradeNation `CHF/JPY`, a multi-word TradeNation
  index name (`Germany 40`) the legacy parser couldn't handle, a canonical
  id (`US30`), and rejects-unknown. 230 cli lib tests pass.

### Verified

- `trade-control calendar-bars --instrument
  "Wall St 30 / Germany 40 Rolling Future Diff" --broker tradenation` — the
  name that previously threw — now resolves, keeps the USD CPI event, and
  writes pause+news bundles.

### Note

- Cargo `version` bumped `0.1.0 → 0.2.0` (root `trade-control-web-hook` and
  `cli/trade-control-cli`).

## v8 — 2026-06-09 — bind Pine alertconditions by title, not positional `plot_N`

### Why

A live `tv-arm` run failed `05-enter` and `06-close-on-reversal` with
`err.code="general"` — the catch-all TradingView returns when an
alertcondition's `plot_N` index doesn't resolve. Root cause: the
`PLOT_LONG_PATTERN`/`PLOT_SHORT_PATTERN`/`PLOT_EVERY_BAR_CLOSE` constants
were positional plot indices, and v2.3's five `next_candle_timestamp_1..5`
plots (added between `recent_low` at plot_9 and the alertconditions) had
silently shifted the three alertconditions from `plot_10/11/12` to
`plot_15/16/17`. The constants were never updated, so the alert payloads
pointed at numeric series instead of alertconditions. The error code is
identical to a stale-compile-cache, so it masqueraded as the
"republish the script" case (which it survived).

### What changed

- **Immediate fix:** corrected the three plot constants to `plot_15/16/17`.
- **Structural fix (the real one):** alertconditions are now bound by their
  **title** (`"Long Pattern"`, `"Short Pattern"`, `"Every Bar Close"`)
  rather than a positional `plot_N`. The `tv-arm` JS template resolves the
  title → live `plot_N` at create time from the study's `metaInfo()`
  (`metaInfo().plots` filtered to `type === "alertcondition"`,
  cross-referenced with `metaInfo().styles[id].title`). Adding or removing
  `plot()` calls in the Pine source can no longer break the binding.
- A title absent from the published study fails that alert **loudly**,
  listing the alertcondition titles it did find — no positional fallback
  (a guessed index is exactly the silent failure this removes).
- Verified against the live chart: the resolver maps the three titles to
  `plot_15/16/17`.

### Breaking

- `conventions`: `PLOT_LONG_PATTERN`/`PLOT_SHORT_PATTERN`/
  `PLOT_EVERY_BAR_CLOSE` and `entry_plot_for`/`reversal_close_plot_for` are
  removed, replaced by `ALERT_LONG_PATTERN`/`ALERT_SHORT_PATTERN`/
  `ALERT_EVERY_BAR_CLOSE` (title strings) and `entry_alert_for`/
  `reversal_close_alert_for`.
- `tv-arm`: `AlertPayload::PineAlertcondition`'s `alert_cond_id` field is
  renamed `alert_cond_title`.

### Config

- None. Operators must keep the alertcondition **titles** in
  `conventions/src/pine.rs` in lockstep with the `alertcondition()` calls
  in `pine-scripts/candle-signals-v2.pine` — but no longer track plot
  indices.

### Tests

- conventions 33, tv-arm 139 — green. Renamed the plot-id asserts to
  title asserts; no positional `plot_N` left in Rust.

### Follow-up

- None outstanding; the plot-index-drift failure class is closed.

## v7 — 2026-06-09 — `--version` reports the git tag/commit

### Why

The CLIs had no useful way to report which build was running. `tv-arm`
exposed clap's `--version` but it printed the never-bumped crate version
(`0.1.0`); `trade-control` had no `--version` at all. After a deploy/build
you want to confirm you're on the version you think you are.

### What changed

- Both `trade-control` and `tv-arm` now report the git tag/commit on
  `--version`, captured at build time via a `build.rs` running
  `git describe --tags --dirty --always` (e.g. `tv-arm v7`,
  `trade-control v7-2-gabc123-dirty`). Falls back to the crate version when
  git isn't available (source-tarball builds).

### Config / Breaking

- None. Adds a `build.rs` to the `cli` and `tv-arm` crates and a
  `GIT_VERSION` compile-time env var.

### Tests

- cli 227, tv-arm 139 — green; `--version` verified to print the describe
  string for both binaries.

## v6 — 2026-06-09 — bake `pip_size` into the signed enter intent

### Why

The worker scales every `offset_pips` into a price with
`price = anchor + offset_pips * pip_size` and binds `pip_size` into the
gate-script scope. For H&S enters that pip came from `pip_size_for`: a
`PIP_SIZE_<instrument>` secret falling back to a forex-shaped `0.0001`
default — silently 100× wrong for JPY pairs and 10000× wrong for indices
unless an operator remembered to set the secret. The worker is WASM and
links no instrument catalog, so it never read the (now-correct)
`instrument-lookup` pip. M/W already solved this by baking pip into the
signed `MwParams`; this extends the same approach to H&S and any non-M/W
enter.

### What changed

- Pip is now baked at arm time and read from the signed intent. Worker
  precedence (`run_enter`): baked `intent.pip_size` → `PIP_SIZE_<instrument>`
  secret → `0.0001` default. The fallback keeps pre-baked in-flight intents
  resolving during rollout.
- `tv-arm` resolves `asset.pip_size` from `instrument-lookup` for the H&S
  path too (previously M/W-only) and bakes it; `--pip-size` override now
  applies to both H&S and M/W.
- `pip_size` is already a gate-script variable (`allow_entry`, `min_r`, …);
  the bound value is now the baked pip.
- No worker-side catalog lookup and no live spread fetch on the hot path —
  pip arrives baked in the signed message.

### Config

- New optional signed field `pip_size` on the enter intent (top-level).
  Absent = the worker falls back to the secret/default (pre-feature
  behaviour); the wire form stays byte-identical when absent.
- `PIP_SIZE_<INSTRUMENT>` secret is now an override/fallback, no longer the
  primary source. Arming through `tv-arm` no longer needs per-instrument
  secrets for JPY pairs or indices.
- New CLI/`TradeSpec` field `pip_size: Option<f64>`.

### Breaking

- None on the wire (additive optional field). `IntentValidationError` gains
  a `PipSizeInvalid` variant; `build_enter_alert` (cli, internal) gains a
  `pip_size` parameter.

### Tests

- core: validate accept/reject (zero/negative/NaN), serde elision +
  round-trip, signed wire round-trip + tamper-rejection, script-visibility
  of `pip_size`.
- cli: H&S enter carries baked pip; omitted when spec has none; M/W enter
  carries matching top-level + `mw.pip_size`.
- tv-arm: H&S spec bakes catalog pip; `--pip-size` overrides on H&S.
- Totals: core 371, cli 233, tv-arm 139, worker 112 — all green; WASM root
  builds.

### Follow-up

- Once all live intents are armed through the updated `tv-arm`, the
  `PIP_SIZE_<instrument>` secrets can be dropped.

## v5 — 2026-06-08 — bar-based pending-order expiry (`expiry_bars`)

### Why

A resting stop-entry whose breakout never happens otherwise sits until
`not_after` (the whole alert window). For a breakout setup, the clean edge
is gone within a few bars — we want to cancel a never-filled order N bars
after placement. Neither broker has a native per-order expiry (TradeNation
orders are hardcoded Good-Till-Cancel; the OANDA worker path uses GTC
too), so the worker must enforce it.

The hard part: "N bars from now" must skip weekends / session breaks, and
a resting order gets **no further webhooks** to count bars from — so the
worker can neither count fires nor (lacking a session calendar) convert
bars→wall-clock across a Friday→Monday gap. Only the indicator can: Pine's
`time_close(timeframe.period, bars_back=-N)` projects forward respecting
the symbol's session schedule.

### What changed

**Wire format (new field + menu)**

- New signed `Intent::expiry_bars: Option<Tunable<u32>>` (1..=5) on the
  enter intent — the author's policy, chosen at arm time.
- New unsigned shell menu `next_candle_timestamp_1..5` (in
  `UNSIGNED_VALUE_KEYS`, routed onto `Shell` in `incoming`) — Pine fills
  the absolute forward bar-close timestamps at fire time. New
  `Shell::next_candle_timestamp(n)` accessor.

**Worker**

- New `core::intent::resolve_cancel_at(expiry_bars, shell, not_after)`:
  picks `menu[expiry_bars]`, falls back to `not_after` on a missing slot,
  caps at `not_after`, and returns `ExpiryError::OutOfRange` for 0 / >5.
- `run_enter` resolves `expiry_bars` (Phase-1 scope, like `max_retries`)
  and computes `cancel_at` **before** any broker work; an out-of-range
  value → `Rejected` 400 `expiry-bars-out-of-range` (does **not** mark the
  id seen — next bar can retry).
- New `EntryAttempt::cancel_at` (additive, `#[serde(default)]`), threaded
  through `retry_gate::record_placement`. Deliberately **separate** from
  `expires_at`, which stays tied to `not_after + grace` (it drives the KV
  row TTL and replay/retry-gate record lifetime — shortening it would age
  records out early).
- Cron sweep: new OR-branch cancels a pending order once `cancel_at` has
  passed, logged `reason=bar-expiry` (distinct from `expired`). Pure
  `bar_expiry_due` predicate added.

**CLI / tv-arm / Pine**

- `TradeSpec::expiry_bars` → threaded onto the `05-enter` intent only.
  `wrap_signed_template` appends the menu placeholders **only when
  `expiry_bars` is set**, so non-expiry trades stay byte-identical and
  don't depend on the new plots.
- `tv-arm --expiry-bars N`.
- `candle-signals-v2.pine` v2.3: five `next_candle_timestamp_1..5` hidden
  plots via `time_close(timeframe.period, bars_back=-k)`.

### Breaking

None. `expiry_bars` absent = today's behaviour (rest until `not_after`);
old KV `EntryAttempt` rows without `cancel_at` decode as `None`.

### Config

- `expiry_bars: <1..5>` on an enter intent / trade spec; `--expiry-bars`
  on `tv-arm`. Requires the v2.3 indicator that ships the menu plots.

### Tests

- core: sig keeps the menu unsigned; incoming routes the menu onto Shell;
  `expiry_bars` round-trips on Intent; `resolve_cancel_at` slot pick /
  out-of-range / missing-slot fallback / not_after cap; `EntryAttempt`
  JSON round-trips with and without `cancel_at` (incl. legacy-row default).
- worker: `bar_expiry_due` predicate; `expiry-bars-out-of-range` outcome
  classifies as Skip (no id poison).
- cli: `expiry_bars` threads onto enter only; menu present/absent in the
  signed body by opt-in; end-to-end sign→substitute→verify round-trip.

### Follow-up

- `on_broker_rejection` recovery (skip/market/limit on `#19-10`, with a
  ≥1R recheck and limit-override) — deferred; brief in
  `BUG-entry-too-close-to-market.md`.
- Pine `time_close` forward projection can't anticipate an *unscheduled*
  one-off holiday inside the window; `not_after` is the backstop.

## v4 — 2026-06-08 — `prep-expire`: a `<prep>-expiry` cutoff line

### Why

An H&S setup is only valid if the break-and-close lands within a bounded
number of bars of the pattern start (M15/H1 30–120, H4 30–180, Daily
30–210, Weekly 30–∞). A real demo trade lost because the break-and-close
came **124 bars** after the pattern start on H1 (max 120) — the pattern
had grown too big to be a clean H&S, but nothing on the chart stopped the
entry. Operators needed a way to draw that cutoff.

### What changed

**`prep-expire` action (new)**

- New `Action::PrepExpire` (wire `prep-expire`). Carries `step` (which
  prep) + `trade_id` + `ttl_hours`. State-only, no broker call.
- New `StateStore` methods `block_prep` / `is_prep_blocked` /
  `clear_prep_block` over a dedicated `prep-blocked:<scope>:<instrument>:<step>`
  keyspace (global-first lookup, account-scoped, TTL-gated — same shape as
  vetos but its own namespace). New `PrepBlockEntry` +
  `Snapshot.prep_blocks` so blocks show in `status`. `PREP_BLOCK_INDEX_CAP`.
- Worker: `handle_prep_expire` stores the block and logs `prep-expire
  stored`; `handle_prep` now rejects a blocked step with a 409
  `prep-expired` and a `prep rejected — expired` log. The rejection is
  `Rejected` (does **not** poison the seen-id, per the 2026-06 replay-scope
  rule), so a re-fire just re-logs. The enter gate's existing
  `missing-prep` log completes the three-line timeline a future debugger
  can grep to reconstruct the trade.
- A prep that already fired *before* the block is untouched — the block
  only stops *future* preps, so a trade that legitimately entered is not
  disturbed.

**Chart side (`<prep>-expiry` line)**

- New drawing label vocabulary: a vertical line `<prep>-expiry`
  (`break-and-close-expiry`, `retest-expiry`, plus `neckline-expiry` /
  `retrace-expiry` aliases). `trade-expiry` keeps its dedicated
  whole-trade-close meaning — a prep named `trade` would collide, but
  that's illogical. `conventions::prep_name_from_expiry_label` resolves
  the canonical prep step.
- New `AlertBasename::PrepExpire(step)` → `08-prep-expire-<step>`.
- CLI `TradeSpec.prep_expiries: Vec<String>` emits one drawing-bound
  `08-prep-expire-<step>` alert per cutoff line. Rejected if a name isn't a
  known prep or is also in `skip_preps`.
- `tv-arm` classifies `<prep>-expiry` lines into `Roles.prep_expiries`,
  binds each to its drawing, and **validates**: a future cutoff with no
  matching prep trend line is a hard error (the setup could never enter);
  a past cutoff is a warning (re-arming later in time).

### Wire / config

- `Intent` gains `action: prep-expire`; `validate` requires `step` +
  `ttl_hours` (`MissingPrepExpireStep`).
- `TradeSpec` gains `prep_expiries` (omitted from serialised yaml when
  empty — byte-identical for existing trades).

### Tests

conventions label-resolution + basename round-trip; core validate (well
-formed / no-step / no-ttl) + block round-trip + account scoping + snapshot
yaml; CLI emitter + reject-unknown + reject-skipped; tv-arm classify +
latest-wins + future-error / past-warn / future-with-prep-ok + alert
binding. Host + wasm build, clippy + fmt clean across all five crates.

### Follow-up

The cutoff timestamp is operator-drawn; nothing yet auto-computes the
bar-count limit per timeframe. A future pass could draw the `<prep>-expiry`
line automatically at `pattern_start + max_bars × resolution`.

## v3 — 2026-05-28 — News-event blackout pauses + drawing-alert hardening

### Why

Macro news events (NFP, CPI, central-bank decisions) cause spike risk that
makes pending H&S setups dangerous to enter during the window. Before this
release, the only way to suppress a single trade across a news event was to
manually veto and remember to clear it. This release adds a first-class
`pause` / `resume` action pair keyed by `(trade_id, blackout_id)` so a
trade can carry multiple concurrent blackout windows independently.

Alongside it, several drawing-alert + signing fixes that had been
accumulating on the working tree: vetos and preps now use a drawing-only
shell (no `{{plot("…")}}` placeholders, which were crashing the worker's
YAML parser when delivered literally), and the signing path covers
`recent_high` / `recent_low` from Pine v2's 2026-05-26 update.

### What changed

**Pause / resume action (new)**

- New `Action::Pause` and `Action::Resume` variants on the `Intent`
  enum, with two new optional fields: `blackout_id` (slug, required on
  pause/resume) and `reason` (free-form label).
- New KV key shape `pause:<trade_id>:<blackout_id>` — pauses are
  per-trade, not per-(account, instrument), so multiple concurrent
  windows on a trade (NFP + central-bank, etc.) coexist as siblings.
- New `StateStore` trait methods: `set_pause` / `list_pauses_for_trade`
  / `clear_pause`. Implemented on both `MemStateStore` (tests) and
  `KvStateStore` (production); listing uses `kv.list` prefix scans.
- Worker dispatch: `Pause` / `Resume` handled in Stage 1, no broker
  call. `run_enter` gains a top-of-pipeline blackout gate that rejects
  with 423 and outcome `paused: [<blackout_id>(<reason>), ...]`
  whenever any pause for the trade is active. Sits ahead of the retry
  gate so a paused trade doesn't burn retry slots.
- New CLI: `trade-control build-pause --from-file <pause.yaml>
  --key-file <key> --output-dir <dir>` emits a signed `01-pause-<id>` /
  `02-resume-<id>` pair plus a `manifest.yaml`. Pure drawing-shell
  alerts — they fire from `LineToolVertLine` time-crosses, not Pine.
- `Snapshot` (the `status` action's response) now includes a `pauses:`
  section listing every active blackout across every trade. Back-compat
  for older serialised snapshots is preserved via serde defaults.

**Python: `tv_arm_hs.py` blackout detection**

- New `BLACKOUT_START_LABELS = {"blackout-start", "pause"}` and
  `BLACKOUT_END_LABELS = {"blackout-end", "resume"}` — interchangeable
  aliases.
- `classify()` collects every matching vertical line into
  `roles.blackout_pairs`. `pair_blackouts()` sorts them chronologically
  and pairs positionally; **odd counts and reversed pairs are hard
  errors that abort the whole run** (including the H&S bundle) — a
  misdrawn chart shouldn't be allowed to arm half a blackout window.
- Per blackout pair, the script writes a `pause.yaml` and shells out to
  `trade-control build-pause`, then maps the resulting `01-pause-*` /
  `02-resume-*` basenames to vertical-line time-cross alerts and stacks
  them onto the H&S `payloads` list for `create_alerts`.

**Drawing-alert + signing fixes (bundled WIP)**

- `wrap_signed_template_drawing` (new) emits a drawing-only shell with
  just `close`/`high`/`low`/`time` placeholders. `wrap_signed_template`
  (renamed concept) keeps the full Pine-bound shell. `trade_patterns`
  picks per-alert: only `05-enter` is Pine-bound; vetos and preps use
  the drawing shell. Fixes 19 rejections/day from `{{plot(...)}}`
  arriving literally and crashing the YAML parser.
- `core::sig::UNSIGNED_VALUE_KEYS` now includes `recent_high` /
  `recent_low` — Pine v2 from 2026-05-26 emits these via
  `{{plot(...)}}`, and the worker treats them as optional shell fields
  for `recent_high` / `recent_low` SL anchoring.
- `IncomingError::BadYaml` and `BadIntentYaml` now carry the underlying
  serde error message so the worker log explains *why* a body was
  rejected. Rejected bodies are also logged in truncated excerpt form
  (cleartext YAML already passes through CF's request log, so no new
  exposure).
- `tv_arm_hs.py` TradeNation instrument resolution now falls back to
  the chart's description ("Germany 40", "Spot Silver") when the raw
  symbol misses the catalog — TN's catalog has FX/stocks but not most
  indices/commodities.
- `build-trade --from-file` now rejects spec accounts that aren't in
  the local CLI history cache, catching typos before they reach the
  worker.

### Breaking

- `Intent` gains two new fields (`blackout_id`, `reason`); both are
  `Option<String>` with `skip_serializing_if`, so the wire form stays
  byte-identical for pre-existing intents. In-tree struct-literal
  callers (8 sites) updated.
- `StateStore` trait gains three new required methods — any future
  out-of-tree implementor will need to add them. All in-tree
  implementors (KV, mem, retry-gate test stub) updated.
- `Snapshot` gains a `pauses: Vec<PauseEntry>` field with
  `#[serde(default)]`; older serialised snapshots still parse.

### Config

- No new env vars or secrets.
- `pause.yaml` spec schema for `build-pause --from-file`:
  ```yaml
  trade_id: eurusd-hs-1            # required, matches parent enter alert
  blackout_id: nfp-2026-06-06      # optional, auto-minted from epoch
  start_time: "2026-06-06T12:30:00Z"
  end_time:   "2026-06-06T13:00:00Z"
  reason: "news:USD-NFP"           # optional, surfaces in seen-index
  instrument: EUR_USD
  account: oanda-reversals-demo
  broker: oanda                    # default
  ```

### Tests

- `core`: 4 new intent validation tests (pause requires trade_id +
  blackout_id, bad blackout_id rejected, well-shaped pair accepted,
  YAML round-trip). 3 new memstore pause tests (round-trip, multiple
  blackouts per trade, isolated per trade_id). Snapshot serialisation
  test extended.
- `cli`: 8 new `pause_pattern` tests including an end-to-end
  build → sign → `parse_and_verify` round-trip with simulated TV
  shell substitution.
- 1 new test confirming drawing alerts emit no Pine plot placeholders.
- All 523 unit tests (306 core + 166 cli + 51 worker) green. Clippy
  clean across all three crates with `--all-targets`. Python script
  syntax-checked.

### Follow-up

- ForexFactory MCP integration: Claude still draws blackout lines
  manually via tv-mcp; a future `tv_draw_blackouts.py` helper could
  automate from FF event data.
- The pause-bundle output directories under `<arm-out>/<sym-date>/pause-N/`
  pile up over time — a janitor pass to prune dirs older than N days
  would help.
- Optional `kv.list(prefix="pause:")` janitor in the worker to expire
  orphaned pauses past N days (today they ride on the alert's
  `not_after + grace` TTL, which is usually enough).
