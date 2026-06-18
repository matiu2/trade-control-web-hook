# TODO — position-tool direct entry (`tv-arm` reads a drawn position)

Goal: `tv-arm-dev` reads a `short_position` / `long_position` drawing on the
TradingView chart and emits a signed `enter` that the worker places
**immediately on receipt** (direct POST to the webhook — not a TV alert, not
the cron engine), using the drawing's entry / SL / TP and direction.

Modes: `--market-entry`, `--stop-entry`, `--limit-entry`.

## Verified facts (from live chart + code)

- Drawing kinds are `short_position` / `long_position` (tv-mcp `draw list`).
- `draw get` returns:
  - `points[0].price` = entry price (absolute)
  - `properties.stopLevel`  = SL distance in **ticks** from entry
  - `properties.profitLevel` = TP distance in **ticks** from entry
- Absolute SL/TP = `entry ± level × tick_size`.
  - short: SL = entry + stopLevel·tick (above); TP = entry − profitLevel·tick (below)
  - long:  SL = entry − stopLevel·tick (below); TP = entry + profitLevel·tick (above)
- `tick_size` comes from `instrument-lookup` `asset.tick_size` — **NOT pip_size**
  (FX tick is 10× finer than a pip; indices/gold tick == pip). Field already
  exists on `instrument_lookup::Asset`.
- Worker direct-POST → immediate `place_entry` works today (HMAC verify → gates
  → broker). Absolute SL and absolute TP both resolve today via
  `PriceRef::Absolute`.
- `EntrySpec::Market` works today with absolute SL/TP — **no core change needed**.
- `EntrySpec::Stop`/`Limit` carry `from: PriceAnchor` (geometry only, no
  Absolute) — an absolute entry **trigger** price is NOT expressible today.
  Stop/Limit modes require a core + worker change.
- A naked enter (no preps, no vetos, single-shot) passes prep/veto/retry/
  allow_entry gates. Still gated by: replay (dup id), cooldown, market-hours
  blackout, spread blackout. `trade_id` is mandatory on every enter.

## Phase 1 — `--market-entry` (no core changes) — IN PROGRESS

- [x] `trading-view/src/drawings.rs`: extend `Properties` with optional
      `stop_level` (`stopLevel`), `profit_level` (`profitLevel`), `qty`.
      Test: deserialize a captured `short_position` payload. (25 tests green)
- [x] `tv-arm/src/roles.rs`: `SHORT_POSITION`/`LONG_POSITION` kinds,
      `PositionDirection`, `PositionDrawing`, `Roles.position`, classify
      geometry-only + latest-wins + half-drawn ignored. Tests added.
- [x] `tv-arm/src/position_trade.rs` (new): `resolve_levels(pos, tick)` →
      `PositionLevels {entry, sl, tp}`; `core_direction`. Tests: DE40
      short/long known values + FX tick≠pip guard. (registered in main.rs)
- [x] `cli/src/trade_patterns.rs`: `TradeSpec.sl_price: Option<f64>`;
      `build_enter_alert` emits `PriceRef::Absolute` SL when set. Test green
      (253 cli tests pass).
- [x] `tv-arm/src/args.rs`: `--market-entry` / `--stop-entry` /
      `--limit-entry` in a mutually-exclusive `position_entry` ArgGroup;
      `--expiry-hours` (default 48); `Args::position_entry_mode()`. Tests.
      (Left existing `--entry-market` for the pattern path untouched.)
- [x] `cli`: `wrap_signed_direct_enter` (self-contained shell w/ drawn
      entry as reference close, no `{{plot}}`); `PositionEnterSpec` +
      `PositionEntryKind` + `build_position_enter` (mints `pos-…` id,
      builds naked enter, signs, returns `(trade_id, body)`; Market only,
      Stop/Limit error pending the wire change). Exported from lib.
- [x] `tv-arm/src/pipeline.rs`: `run_position_entry` — early dispatch on
      `position_entry_mode()`, resolves levels via `tick_size`, expiry
      (vertline else `--expiry-hours`), build+sign, write body for audit,
      **direct-POST** via `post_intent_blocking`.
- [x] README: documented the three flags + direct-POST behaviour + the
      tick_size vs pip_size note + the Market-only status.
- [x] clippy + fmt + tests green (tv-arm 240+13, cli 170, trading-view 25;
      full workspace builds). Commit + push.

**KEY DESIGN NOTE (verified against live chart + worker code):** the
position tool stores entry as an **absolute** price but SL/TP as **tick
offsets** (`stopLevel`/`profitLevel`). The worker's resolver range-checks
`stop_loss < close < take_profit` (long) using the signed shell `close`
and derives the R-multiple from it — so the direct-POST shell must carry
the **drawn entry price** as `close`, not a placeholder zero (which would
be rejected `EntryOutsideRange`). `wrap_signed_direct_enter` does this.

## Phase 2 — `--stop-entry` / `--limit-entry` (core + worker)

- [ ] `core/src/intent.rs`: let `EntrySpec::Stop`/`Limit` express an absolute
      trigger price (make `from` a `PriceRef`, or add `at: Option<f64>`).
      Update `Intent::validate`.
- [ ] `core/src/intent/resolution.rs`: resolve absolute entry trigger.
- [ ] `cli`: add `EntryMode::Limit`; plumb absolute entry price.
- [ ] `tv-arm`: wire `--stop-entry`/`--limit-entry` to the absolute entry price
      from the drawing's `points[0].price`.
- [ ] Worker + resolution tests.
- [ ] Deploy to dev (`./deploy-dev.sh` on `main`), verify against live chart.

## Open questions / notes

- Direction is read from the drawing kind. Cross-check entry vs SL side as a
  sanity assertion (short ⇒ stop above entry).
- `trade_id` minted by the build path (single-shot, one placement).
- Keep each phase < 600 lines.

---
(Prior branch-base TODO for the unrelated market-hours-blackout work was
overwritten here; it remains intact on its own branch.)
