# Changelog

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
