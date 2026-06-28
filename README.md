# Trade Control Web Hook

Cloudflare Worker that receives TradingView alerts and controls OANDA /
TradeNation trades. The body is cleartext YAML with an HMAC-SHA256
signature, so a leaked webhook URL can't be weaponised by anyone who
doesn't also have the signing key.

Fifteen actions are supported. The first five are the day-to-day trading
verbs; the rest are state management for multi-event setups and scheduled
windows, plus the journaling-cleanup purge commands.

Trading:

- `enter` — open a market, stop, or limit order with SL/TP, after passing the risk gate.
  Optionally gated on named `prep` / `veto` flags (see "Conditional entries" below).
- `close` — close all positions for the instrument. May also carry worker-side
  gates that decide whether *this* close fires:
  - **Contextual-window gate** (OR-composed): `inside_window: [news, price]`
    names which window-types are acceptable; `sr_bands: [[lo, hi], ...]`
    carries the data for the `price` member. The two fields are paired —
    `price` ∈ `inside_window` iff `sr_bands` is non-empty. The close
    passes when *any* listed window matches (active news window for the
    `trade_id`, or current broker price inside a band).
  - **Candle-quality gate** (AND-composed with the window): `needs_golden:
    true` or `needs_confirmed: true` requires the incoming shell to carry
    `golden: true` / `signal_confirmed: true` from the Pine study.
    `needs_golden` is the default emitted by the CLI's reversal-close
    builder; `needs_confirmed` is the operator opt-in to "confirmed but
    not necessarily golden".
  - **Ad-hoc filter** (AND-composed): `allow_close: <Rhai script>` —
    symmetric with `allow_entry` but bound to the shell-anchor scope
    only (no resolved SL/TP geometry to read).
  - **Veto-on-reversal hook** (experimental, default off): `veto_on_reversal:
    true` on a price-windowed reversal-close. When the close gate passes,
    the worker *also* writes a `reversal` veto for this `trade_id`, so a
    *later* `enter` for the same setup is blocked by the entry-side veto
    gate. The motivating case: a reversal off support/resistance that lands
    **before** the entry fires — today the close is a no-op (no position
    yet) and the entry goes in anyway, even though the reversal was a strong
    "this trade won't work" signal. The veto is StopNextEntry-style: it only
    blocks future entries, never force-closes beyond the close this intent
    already performs. Written on every gate-pass (idempotent, TTL = life of
    the alert window). Requires a price window (`inside_window` ∋ `price` +
    `sr_bands`, or the deprecated `require_price_in_ranges`) and a
    `trade_id`; rejected at validate time otherwise. The worker only checks
    veto names an `enter` lists in its own `vetos`, so when this hook is
    armed the CLI/tv-arm pipeline **also adds `reversal` to the matching
    `05-enter`'s `vetos`** — both halves move together. Arming the close
    flag by hand without the enter half writes a veto nothing reads.
  - **Deprecated form** (still accepted for in-flight alerts):
    `require_news_window: true` and/or `require_price_in_ranges: [[lo, hi], ...]`.
    Mixing the old and new forms on one intent is a validation error —
    pick one. Migrate to the new form on next regen.
  With no gate set the close is unconditional (operator emergency-close path).
- `invalidate` — set a per-instrument cooldown (default 12 h) and cancel any pending
  orders. Use this when your setup is no longer valid (price drifted out of the
  expected range) and you want to be sure no entry fires while you sleep.
- `status` — read-only snapshot of active cooldowns, recent seen ids, preps, and
  vetos. Curl-friendly debugging.
- `market-info` — read-only query: return TradeNation's per-instrument market
  details for the intent's `instrument` (trading session hours in Brisbane +
  London, spread, margin, guaranteed-stop terms, expiry). Unlike the other
  control actions this needs a live TradeNation broker (its `market_info` call
  isn't on the generic `Broker` trait), so it dispatches through the broker path;
  it still records `seen` and is fully idempotent. **TradeNation only** — a
  non-TN intent is rejected `400`. These hours feed the upcoming market-hours
  entry blackout.
- `plan-list` — read-only: list the registered server-side `TradePlan`s, each
  with a compact summary of its current `PlanState` (phase, watermark, fired
  rules, shadow flag). The `ACCOUNT` column reflects the plan's KV scope
  (`plan:{account}:{trade_id}`); `tv-arm` registers each plan under its
  resolved `--account` so the plan shares the scope of its own vetos/preps. A
  plan armed without an account (legacy / global `_` scope) shows `-`. Lists
  live plans by default; the intent's `include_archived` flag (CLI
  `--include-all`) also lists terminated (vetoed/completed) plans retained in
  the archive keyspace. Drives `trade-control plan list`. KV-only, idempotent.
- `plan-show` — read-only: dump one plan in full (every rule + its persisted
  `PlanState`). Target named by the intent's `trade_id`; the worker scans all
  account scopes — **live and archived** — so a terminated plan surfaced by
  `plan list --include-all` is still inspectable (an archived match carries an
  `archived_at` field). Drives `trade-control plan show <trade_id>`. KV-only.
- `plan-delete` — drop a registered plan and its `PlanState` — the inverse of
  `register`. Target named by the intent's `trade_id`; the worker scans all
  account scopes and deletes the matching `plan:` + `plan-state:` rows **and**
  any matching `archived-plan:` row (so a terminated plan can be cleared after
  analysis). Drives `trade-control plan delete <trade_id>`. KV-only, idempotent
  (deleting a missing plan is a no-op). Use to re-arm a setup after editing its
  chart, or to clear an archived plan once analyzed.
- `plan-purge` — wipe **every** trace of one journaled trade. A superset of
  `plan-delete`: it scans all account scopes and removes the per-trade KV rows
  (`plan:` / `plan-state:` / `archived-plan:`, plus the trade's
  `entry-attempt:` rows and each attempt's `order-body:`, the `control-event:`
  audit trail, and any enumerable trade-scoped `pause:` / `news:` rows) **and**
  the trade's R2 `ticks/` bundles. Window-TTL'd `veto:` / `prep:` rows are left
  to self-clear (their lifecycle is already in the control-event trail). Target
  named by the intent's `trade_id`. Drives `trade-control plan purge
  <trade_id>`; idempotent.
- `purge-older-than` — bulk R2 retention sweep. Deletes recorded bundles under
  both the `req/` and `ticks/` prefixes whose date prefix is older than a cutoff
  (carried as N days on the intent). KV is untouched. Drives `trade-control
  purge --older-than <days>`.
- `unlock` — clear the cooldown for one instrument. Recovery for an
  `invalidate` you didn't mean to send.

Per-instrument flag state (TTL-gated):

- `prep` — record a named step (e.g. `break-and-close`) for an instrument with a
  TTL, used to build up multi-event setups.
- `prep-expire` — block all *future* `prep` fires for one named `step` on an
  instrument (KV flag `prep-blocked:<scope>:<instrument>:<step>`, TTL-gated). Once
  set, any later `prep` for that step is rejected, so an entry whose
  `requires_preps` lists the step can never open. A prep that already fired
  *before* the block is untouched. No broker call. Fired by a `<prep>-expiry`
  chart line when the window for landing the prep has lapsed (e.g. an H&S
  break-and-close that never came within the allowed bar count).
- `veto` — record a named blocker (e.g. `news-window`) for an instrument with a
  TTL. Carries an optional `level`:
  - `stop-next-entry` (default) — KV flag only; future entries that opt in via
    `vetos: [name]` get rejected. No broker call.
  - `cancel-pending` — also cancels resting stop / limit orders on the
    instrument.
  - `close-positions` — also closes any open positions on the instrument.
  In all cases the flag survives until TTL / `clear-veto`. Re-firing a level-2
  or level-3 veto re-runs the broker side effects.
- `clear-prep` / `clear-veto` — drop a single prep or veto flag before its TTL
  expires.

Per-trade window state (paired alerts):

- `pause` / `resume` — open/close a blackout for a `(trade_id, blackout_id)`.
  While any pause is active the `enter` gate on that `trade_id` is blocked.
  Used to bracket scheduled news events without invalidating the whole setup.
- `news-start` / `news-end` — open/close a news window for a
  `(trade_id, news_id)`. Independent of `pause`: news windows don't block
  entries, they **enable** the `06-close-on-reversal` alert (a Close intent
  with `news` in its `inside_window` list) to flatten the trade on an
  opposing reversal candle.

## Alert basenames emitted by `build-trade`

When `tv-arm` (the chart-arming binary) calls `trade-control build-trade
--from-file`, the Rust CLI mints a fixed-order bundle of signed YAMLs.
Basename ordering matters — `tv-arm` maps drawings to alerts by prefix.

| Basename | Action | Fires on | Notes |
|---|---|---|---|
| `01-veto-too-high` | `veto` | Horizontal line crossing | Invalidation veto, level `close-positions`. Drawing-bound. Trade-direction sensitive (`too-low` for bullish IH&S). Fires when price runs back past the right shoulder → structure broken, so an open trade is flattened. |
| `01-veto-too-low` | `veto` | Price crossing pcl-exhausted level | "Pattern completion level exhausted" — 80% of the way from the fib's midpoint to TP. Value-bound, computed by `tv-arm` from the fib geometry. Direction-mirrors the invalidation veto, but level is `stop-next-entry`, **not** `close-positions`: a pcl breach is in the trade's favour (price ran toward TP), so it only blocks a *late* entry and never touches an open position. (Was wrongly `close-positions` until the trade-046 fix — it closed an in-profit short ~31 ticks early.) |
| `02-veto-trade-expiry` | `veto` | Vertical line crossing chart time | Hard stop: once the trade-expiry line passes, no more entries. |
| `03-prep-break-and-close` | `prep` | Trendline crossing (neckline break) | Skippable for stocks / late entries with `--skip-break-and-close`. |
| `04-prep-retest` | `prep` | Trendline crossing (retest from below) | Skippable with `--skip-retest`. |
| `05-enter` | `enter` | Pine `Candle Signals` golden candle | The actual trade. Gated on the preps above + opposing-direction veto absent. |
| `09-enter-qm` | `enter` | Pine `Candle Signals` (same detector as `05-enter`) | **`tv-arm --strategy-v2` only.** The Quasimodo entry, armed alongside `05-enter`: no preps, confirmed-candle gated. Its entry spec is **identical to standalone `--quasimodo`** — a stop at signal_low − the ATR buffer (`offset_atr_pct: 0.5`; see "ATR buffer") with a `recover_entry: limit` fallback (fills on the pullback when the level was overrun), *not* a bare limit. Shares the trade's `trade_id` + `max_retries` with `05-enter`; first of the two to fire cancels the other's resting order (worker retry gate). See "Dual entry — `--strategy-v2`" below. |
| `06-close-on-reversal` | `close` | Pine `Candle Signals` opposing reversal | Emitted when news-pairs and/or `support`/`resistance` lines are drawn. Carries `inside_window: [news?, price?]` (OR-composed) and, when `price` is listed, `sr_bands: [[lo, hi], ...]`. Defaults `needs_golden: true` for the candle-quality gate. With `tv-arm --veto-on-reversal` (experimental) it also carries `veto_on_reversal: true`, so a reversal off a band before entry vetoes the upcoming trade — see the `close` action notes above. |
| `08-prep-expire-<step>` | `prep-expire` | Vertical line crossing chart time | Emitted once per chart-drawn `<prep>-expiry` line (`break-and-close-expiry`, `retest-expiry`). When crossed, blocks any further `<step>` prep on the trade — so a setup whose prep lands too late never enters. Drawing-bound. `<step>` is the canonical prep name and may contain hyphens. |

The legacy `07-close-on-sr-reversal` basename is no longer emitted —
its functionality folds into a single `06-close-on-reversal` whose
`inside_window` list includes `price`. The enum variant is still
recognised by the worker for inbound decode of any in-flight alerts
left over from the old shape.

Each news pair adds two more (`01-news-start-<id>` + `02-news-end-<id>`)
via a separate `build-news` shell-out, and each pause pair adds
`01-pause-<id>` + `02-resume-<id>` via `build-pause`.

### M/W (double-top / double-bottom) bundle

M (double-top → short) and W (double-bottom → long) reversal setups
emit a **different, smaller** bundle — no prep chain, single-shot:

| Basename | Action | Fires on | Notes |
|---|---|---|---|
| `01-veto-mw-cancel` | `veto` | Price crossing the 1.3-extension of the neckline→peak leg (**intra-bar, on first tick**) | Level `cancel-pending`. The same 1.3 extension the second peak must stay within, so it doubles as the two-peaks alignment ceiling. Cancels the resting entry + disarms. Value-bound, computed by `tv-arm`. |
| `01-veto-mw-abort` | `veto` | A candle **closing** back through the neckline | Level `cancel-pending` (matters only while pending — once filled the trade rides its SL/TP). Value-bound at the neckline. |
| `01-veto-mw-overshoot` | `veto` | Price reaching **180% of the top→neckline leg** (= 80% of the way neckline→TP) (**intra-bar, on first tick**) | Level `cancel-pending`. The projected move is essentially complete, so a fresh entry's R:R no longer justifies opening — cancels the resting entry + disarms, never closes an open position. Value-bound at a **static** arm-time price; as the pattern grows it only over-vetoes (the safe direction). M fires on a **low** reaching it, W on a **high**. |
| `02-veto-trade-expiry` | `veto` | Vertical line crossing chart time | Same hard stop as H&S. |
| `05-enter` | `enter` | Pine **`Every Bar Close`** alertcondition (every closed bar, not the golden/short-pattern plots) | Carries the baked static M/W params (`mw:` block); the **worker** computes entry/SL/TP from those + the live shell OHLC each bar, mid→bid/ask corrected with the arm-time spread. `max_retries: 0`, no preps. |

There is **no `06-close-on-reversal`** for M/W — the take-profit is a
hard 1R, so there's no opposing-reversal close to arm.

### Position-tool direct entry (`--market-entry` / `--stop-entry` / `--limit-entry`)

For a manual trade you've already framed with a TradingView **position
tool** (the long/short risk-reward rectangle), `tv-arm` can skip the
whole pattern machinery and place the trade straight from the drawing.

- Draw a `long_position` / `short_position` tool on the chart: drag the
  entry, stop, and target to where you want them. (Optionally draw a
  `trade-expiry` vertical line; otherwise `--expiry-hours` applies,
  default 48h.)
- Run `tv-arm-dev --market-entry` (or `--stop-entry` / `--limit-entry`).
  Exactly one of the three may be set.

`tv-arm` reads the tool's entry anchor (`points[0].price`) and its
`stopLevel` / `profitLevel` — which TradingView stores as **tick
distances**, not absolute prices — and converts them to absolute SL/TP
via the catalog **`tick_size`** (from `instrument-lookup`, *not*
`pip_size`; for FX the tick is 10× finer than a pip). Direction comes
from the tool kind (short ⇒ stop above entry, target below).

Unlike the pattern bundles, this path does **not** post a TradingView
alert. It builds one signed `enter` intent (direction + `EntrySpec`,
absolute `stop_loss` / `take_profit` as `PriceRef::Absolute`, the drawn
entry as the signed shell reference price, the trade-expiry as
`not_after`) and **POSTs it directly to the worker**, which places the
order on receipt. No preps, no pattern vetos — it's a naked manual entry,
still subject to the worker's replay / cooldown / market-hours /
spread-blackout gates. `--risk-amount`, `--broker-dry-run`, and
`--pip-size` apply as usual.

**Order types:**

- `--market-entry` — market order, filled by the worker on receipt at
  broker price.
- `--stop-entry` — pending **stop** order resting at the drawn entry
  price (breakout: it triggers when price trades *through* the level).
- `--limit-entry` — pending **limit** order resting at the drawn entry
  price (pullback: it fills when price comes *back* to the level).

The resting price is baked onto the wire as an absolute trigger
(`EntrySpec::{Stop,Limit}::at`) — the operator drew the exact level, so
the worker uses it verbatim rather than re-deriving it from a shell
anchor. The usual "wrong-side" geometry guard (which would reject a long
stop at/below the reference price) is **skipped** for an `at` trigger,
because the signed shell carries that same drawn level as its reference;
the broker, not the worker, arbitrates whether a resting order is
wrong-side and rejects it if so.

## General workflow

The day-to-day loop, end to end:

1. **Draw the setup on a TradingView chart.** Mark the invalidation line,
   the neckline (break-and-close prep), the retest, a fib retracement
   spanning head → neckline, a `trade-expiry` vertical, and any optional
   extras (`news-start`/`news-end` pairs around scheduled news,
   `support`/`resistance` horizontals near key levels).
2. **Run `tv-arm`.** The Rust binary (`cargo run -p tv-arm --`) reads
   the chart geometry via tv-mcp, shells out to `trade-control
   build-trade --from-file` for `trade_id` minting + signing, then posts
   every signed alert into TradingView via an inside-page `fetch()`.
   Each alert lands as a configured TV alert pointed at your worker URL.
   (The legacy `scripts/tv_arm_hs.py` is deprecated; `tv-arm` superseded
   it.) Pine alertconditions (`05-enter`, `06-close-on-reversal`) are
   bound by their **title** (`"Long Pattern"`, `"Short Pattern"`,
   `"Every Bar Close"`), not a positional `plot_N` id: the tv-arm JS
   resolves title → live `plot_N` from the study's `metaInfo()` at
   create time, so adding/removing plots in the Pine source can't break
   the binding. A title that isn't on the published study fails that
   alert loudly with the list of titles it did find — republish the
   study or fix the title in `conventions/src/pine.rs`.
3. **TradingView fires alerts** as their conditions trigger (line
   crossings, Pine `Candle Signals` plots, time anchors). Each alert
   POSTs the cleartext signed YAML to the worker.
4. **The worker verifies the HMAC**, runs replay protection (the `id`
   field), applies any relevant gates (preps must be set, vetos must
   be clear, `inside_window` entries OR-composed for closes, candle-
   quality gates AND-composed on top), then dispatches to OANDA or
   TradeNation. Outcomes are visible in Cloudflare Real-time Logs and
   via `trade-control status`.
5. **The scheduled `cron` triggers** (declared in `wrangler.toml`,
   dispatched on `event.cron()` in `src/cron.rs`):
   - `*/15 * * * *` — sweeps pending stop-entry orders for SL-breach /
     bar-expiry independently of any TV alert, runs the
     spread-recovery watcher (see below), **and** runs the
     server-side trade-plan engine (`run_engine_tick`, see below).
   - **Server-side engine** (experimental, dev only) — on each `*/15`
     tick the engine enumerates every registered `TradePlan` (see
     `--register-plan`), fetches the broker candles closed since each
     plan's watermark, runs the per-trade FSM evaluator, and dispatches
     any fired intents through the *same* `run_enter` / `run_close` /
     veto handlers the webhook uses — unless the plan is registered with
     `--shadow`, in which case it evaluates and advances state but only
     *logs* its would-be fires (the safe way to run beside the live TV
     alerts; a live plan would double-fire — see `--register-plan`). It
     runs **in parallel** with the TV alerts until proven on demo; the
     `*/15` cadence stays for now. A plan's first tick *seeds*
     its watermark without firing, so conditions already true at register
     don't back-fire. Both strategy families are now evaluated
     server-side: **M/W** fires the enter heartbeat every closed bar
     (`run_enter` owns the live neckline geometry), and **H&S** fires its
     `PinePattern` enter from the Rust port of the `candle-signals-v2.pine`
     detector (pinbar / tweezer / double-tweezer / regular- &
     floating-engulfer, plus the pending→valid→invalid confirmation state
     machine). A fired H&S enter carries the latched signal geometry
     (`signal_high` / `signal_low` / `golden` / `signal_confirmed` /
     `recent_*` / `atr`) onto its shell, so it resolves entry/SL/TP against
     the *pattern* extremes exactly as the TV alert's `{{plot(...)}}`
     substitutions did. The port confirms only on **fully-closed** pushing
     bars (the engine never sees an unclosed bar), which fixes the Pine
     one-bar-early confirm timing (bug #10B). Confirmation also resolves
     **only at the end of the `confirm_bars` window** (v2.6): a push through
     the signal's extreme anywhere inside the window is latched, and the
     signal validates when the window closes iff such a push occurred — it no
     longer fires the instant a bar breaks through. Both `candle-signals-v2.pine`
     and the Rust port carry this fix in lock-step. Validation against
     recorded Pine fires by historical replay is a tracked follow-up.
     The **`06-close-on-reversal`** close is the *exit* side of the same
     detector: it's a `PinePattern` guard bound to the *opposite* direction
     (a long reversal candle closes an open short, and vice-versa). When that
     reversal prints **and** its close sits inside one of the intent's
     `sr_bands` (the pure half of `run_close`'s contextual window — the
     news-window half stays a dispatch-time KV check), the engine fires the
     close, which flattens the position and retires the plan. Before this it
     was inert in the cron-engine era (the engine never ran Pine detection for
     guards, only for the enter), so a trade that should have exited on a
     reversal candle over-held to SL/TP/window-end; the offline `replay-candles`
     simulator now honours the close fire too (see *Candle replay* below).
   - `5 21 * * *` **and** `5 22 * * *` — the daily **NY-close-edge**
     check for the spread-blackout feature. CF crons are UTC-only and
     can't carry a timezone, so both candidate minutes fire (21:05 UTC
     covers New York's EDT close, 22:05 UTC covers EST). The handler
     re-checks `is_ny_close_edge(now)` in Rust, so the wrong-season fire
     no-ops and the one-hour DST shift is decided in code, not by the
     schedule. See the "Spread-blackout window" note below.
6. **End of trade:** the trade-expiry vertical fires the
   `02-veto-trade-expiry` alert, which sets an invalidation veto that
   blocks any future `05-enter` for that `trade_id`. Pauses and news
   windows for the trade auto-expire at trade-expiry (their KV TTLs are
   tied to the alert's `not_after`).

For ad-hoc one-off trades you can skip step 1–2 and use the Rust
`trade-control sign` CLI directly (see "Signing an intent" below).

## How it works

TradingView only substitutes a fixed set of `{{...}}` placeholders into
the alert body. So the body is a flat YAML document with the
TradingView shell at the top and the intent fields next to it, ending
with an HMAC over the whole thing:

```yaml
# TradingView fills these at delivery time
close: {{close}}
high:  {{high}}
low:   {{low}}
time:  "{{time}}"
# Intent fields, cleartext — pasted from the CLI's `sign` output
v: 1
id: pin-bar-eurusd-2026-05-13-a
action: enter
instrument: EUR_USD
direction: long
entry: { type: market }
stop_loss:   { from: low,  offset_pips: -2 }
take_profit: { from: close, offset_r: 2.0 }
risk_pct: 0.5
not_after: "2026-05-14T02:00:00Z"
# HMAC-SHA256 over the canonical form of the above
sig: "v1-sig.<base64>"
```

The signature covers the *schema fingerprint* (the sorted set of
top-level keys) plus the *values* of every key except the four shell
keys — TradingView substitutes those at delivery time and they can't be
known at sign time. The schema fingerprint catches added / dropped /
renamed top-level fields even though their values aren't signed. See
`core::sig` for the exact canonical form.

SL/TP rules reference the plaintext shell prices by anchor with a pip
offset, so the CLI never needs to know the live price — TradingView
fills it in at fire time. Valid anchors:

- `close` / `high` / `low` — the triggering candle's own values.
- `recent_high` / `recent_low` — the indicator's `sl_lookback` window
  (bars *strictly before* the signal bar). An SL anchor that doesn't
  depend on the signal candle's own wick.
- `signal_high` / `signal_low` — the *latched pattern extreme* (e.g. an
  H&S head / right-shoulder). Unlike `high`/`low`, these are stable
  across a confirmation re-fire, so an H&S/IHS enter resolves to the
  same entry/SL geometry on the break-candle fire and the confirmed
  re-fire. This is the default SL/entry anchor for the H&S/IHS builders.

`recent_*` and `signal_*` fall back to the candle's own `high`/`low`
when an older Pine indicator didn't ship the field.

Why no encryption? The intent isn't secret — only its authenticity
matters. Cleartext lets the operator inspect what TradingView actually
sent via Cloudflare's request log, which makes debugging vastly easier
than chasing decrypt errors.

## Intent YAML

```yaml
v: 1
id: pin-bar-eurusd-2026-05-13-a       # unique per intended trade
not_before: "2026-05-13T12:00:00Z"    # optional
not_after:  "2026-05-14T02:00:00Z"    # hard expiry, required
action: enter                          # enter | close | invalidate
instrument: EUR_USD
direction: long                        # long | short
entry: { type: stop, from: signal_high, offset_atr_pct: 0.5 }
                                       # or { type: market }
                                       # or { type: limit, from: low, offset_atr_pct: 0.5 }
                                       # offset_atr_pct = % of ATR (preferred, volatility-scaled);
                                       # offset_pips still works (deprecated). See "ATR buffer" below.
                                       # stop entries may add recover_entry: see below
stop_loss:   { from: signal_low, offset_atr_pct: 0.5 }   # anchored — or { absolute: 1.86236 }
take_profit: { from: close, offset_r: 2.0 }    # 2R — or { absolute: 1.86899 }
                                       #         or { from: high, offset_pips: 50 }
risk_pct: 0.5                          # % of NAV; capped server-side
min_r: 1.0                             # optional. Defaults to 1.0. Worker
                                       # rejects if (TP-entry)/(entry-SL)
                                       # falls below this. Overrides must
                                       # be >= 1.0 — values below the floor
                                       # are rejected both at the encoder
                                       # and on the server.
expiry_bars: 3                         # optional, 1..=5. Cancel a resting
                                       # stop/limit order if it hasn't filled
                                       # within N bars. See "Bar-based order
                                       # expiry" below. Omit to rest until
                                       # not_after.
blackout_close: cancel_resting         # optional. What the market-hours
                                       # blackout sweep does with a still-
                                       # resting order caught in the daily
                                       # close→open gap. cancel_resting
                                       # (default) cancels the unfilled order
                                       # only; cancel_and_close also flattens
                                       # an open position. See "Market-hours
                                       # entry blackout" below.
cooldown_hours: 12                     # only used by "invalidate"
breakeven: { threshold: 0.5 }          # optional. Move the SL to break-even
                                       # (the entry price) once a candle CLOSES
                                       # past this fraction of entry→TP. Latched
                                       # / one-way. Default ON at 0.5 when armed
                                       # via build-trade / tv-arm; omit to keep
                                       # the static SL. See "Break-even stop
                                       # management" below.
```

`take_profit` can also be `{ from: high, offset_pips: 50 }` for a fixed
anchored TP. `offset_pips` is in instrument pip units, scaled by the
instrument's pip size to a price.

**ATR-based buffer (`offset_atr_pct`) — preferred over `offset_pips`.**
Every anchored offset (entry trigger, `stop_loss`, anchored `take_profit`)
accepts `offset_atr_pct` instead of `offset_pips`. The buffer then resolves
**at fill time** as `(offset_atr_pct / 100) × ATR`, where ATR is the Wilder
ATR the signal detector already latches for the firing bar (per-timeframe
length — 24 on H1, 96 on M15, …). So a quiet pair gets a tight buffer and a
noisy instrument (Wheat, an index) gets a proportionally wider one from the
*same* setting — no per-instrument hand-tuning. New H&S / iH&S enters armed
through `tv-arm` / `build-trade` default to **`offset_atr_pct: 0.5`** (0.5% of
ATR) on both entry and SL, replacing the old hardcoded ±1 pip.

- `offset_atr_pct` is an **unsigned magnitude** — the direction comes from the
  anchor (`*high` anchors push the level up, `*low` anchors push it down, away
  from the candle). (Contrast `offset_pips`, which carried its own sign.) A
  `close` anchor has no "away" side, so `offset_atr_pct` on `close` is rejected.
- `offset_pips` and `offset_atr_pct` are **mutually exclusive** on one offset;
  setting both is rejected at parse time and at resolve.
- **Fail-closed on warmup.** If an `offset_atr_pct` offset resolves on a bar
  with no ATR yet (the warmup region, fewer closed candles than the ATR length,
  or a short broker feed), the enter is **declined this bar** rather than placed
  with a zero buffer — the plan stays armed and the next bar (with a warm ATR)
  retries. A golden enter can't validly fire in warmup anyway.
- **`offset_pips` is deprecated** but still honoured for in-flight / hand-armed
  plans and for an explicit pip buffer (set it to opt back in). The same buffer
  resolution lives in `trade_control_core`, so the live worker and the offline
  replay resolve it identically.

**Pip size precedence.** The worker resolves the pip size from, in order:

1. the **`pip_size`** field baked into the signed enter intent — `tv-arm`
   sets this from `instrument-lookup` (`asset.pip_size`) for both H&S and
   M/W enters, so JPY pairs (`0.01`) and indices (`1.0`) size correctly
   without any per-instrument config. This is the authority and is covered
   by the signature (tampering it fails verification);
2. the **`PIP_SIZE_<INSTRUMENT>`** secret — an explicit operator override /
   fallback for intents armed outside `tv-arm`;
3. the **`0.0001`** forex default — last resort.

When you arm through `tv-arm` you no longer need to set `PIP_SIZE_` secrets
for JPY pairs or indices; the correct pip is baked in. The baked `pip_size`
is also bound as a variable in gate scripts (`allow_entry`, `min_r`,
`risk_pct`, …) alongside `entry_price`, `r_multiple`, etc.

**How a catalog change reaches each binary.** The `instrument-lookup`
catalog (`instrument-lookup/src/catalog.toml`) is `include_str!`-compiled
into the `instrument-lookup` library, which the **CLIs** (`tv-arm`,
`tv-news`, `trade-control`) link directly. So:

- **The worker never links `instrument-lookup`** — it isn't wasm-clean
  (its default `import` feature pulls in `reqwest` blocking) and was
  deliberately kept catalog-free. **Recompiling/redeploying the worker
  does *not* teach it a catalog change.** The worker only sees catalog
  facts (`pip_size`, …) that `tv-arm` baked onto each *signed intent* at
  **arm time**. To make a catalog edit affect a live setup you re-**arm**
  it (re-run `tv-arm` with the new catalog) — you do not redeploy the
  worker.
- **The CLIs do pick up a catalog edit on rebuild.** Change
  `catalog.toml`, rebuild/reinstall the CLIs (`./deploy-*.sh` does this as
  part of a deploy), and they resolve against the new baseline.
- **Caveat — the CLIs also merge a runtime overlay.** On every invocation
  they merge `~/.config/instrument-lookup/mappings.toml` over the baked
  baseline (same `id` replaces, new `id` appends). So a baked CLI is *not*
  fully self-contained: an overlay there can override the compiled-in
  catalog with no recompile. To add/override a single asset for an
  operator, editing that overlay is the no-rebuild path; editing
  `catalog.toml` is the everyone-gets-it, requires-rebuild path.

**Stop vs limit entries:** a `stop` order fills when price moves *through*
the level (breakout: long stops sit *above* current price, short stops
*below*). A `limit` fills when price comes *back* to the level (pullback:
long limits sit *below* current price, short limits *above*). The worker
rejects the trade if the geometry is wrong (e.g. a long limit priced above
the current candle close), so a typo can't turn a limit into an instant
market fill at a worse price.

**Wrong-side stop-entry recovery (`recover_entry`):** a stop entry can be
unplaceable because its trigger has already been overtaken by price. This
happens at **two** points:

1. **At resolve time** — the breakout ran *during* the
   signal-confirmation wait, so by the bar the signal confirms the stop
   is on the wrong side for its direction (a short stop now sits at/above
   the close, a long stop at/below). Previously the resolver returned
   "geometry inconsistent with direction" and the entry was **silently
   dropped**, even when the thesis was right and price ran to TP.
2. **At the broker** — the order tries to rest and TradeNation rejects it
   as "entry too close to / on the wrong side of the market" (`#19-10`).
   By default the placement fails (HTTP 502) without poisoning the intent
   id, so the next signal bar can retry.

A `stop` entry can opt into a recovery for **both** cases:

```yaml
entry:
  type: stop
  from: signal_low
  offset_pips: -1.0
  recover_entry:              # optional; default = skip (today's drop)
    action: limit             # market | limit | skip
    max_slippage_pips: 8.0    # optional for market; derived if omitted
```

- `action: skip` (default, also when `recover_entry` is omitted) — drop
  the entry (resolve-time) / fail the placement without poisoning the id
  (broker-time), letting the next bar try.
- `action: market` — enter the confirmed breakout at **market**. At
  resolve time the reference becomes the current candle close; at the
  broker the live market price. The chase is bounded by
  `max_slippage_pips`, or — when omitted — by the **derived SL→entry
  distance** (`|stop_loss − trigger|`), so a runaway breakout can't be
  chased into a much worse fill without the operator supplying a number.
  The recovered entry is re-sized against the actual fill reference.
- `action: limit` — rest a **limit order** at the original stop trigger
  and wait for price to pull back to the intended entry. This preserves
  the planned R exactly (a limit can't fill worse than its price) at the
  cost of possibly never filling. A geometry guard applies: the limit
  must rest on the correct side (long: trigger at/below the close; short:
  at/above) — a degenerate case that would create a `#19-9` is dropped.
  The resting limit is a normal pending order, so the alert-window /
  `expiry_bars` cron sweep cancels it if it never fills.

**Both recoveries are still gated.** The recovered entry runs through the
same resolver tail as any entry, so the **≥1R floor** (`min_r`) and the
**in-range** check (entry strictly between SL and TP) re-run against the
new reference — a recovery that's too far toward TP (low R) or already
past a level is refused. The worker's **SL≥10×spread** floor also still
applies. The broker re-place is a **single** synchronous attempt and does
**not** consume a multi-shot `max_retries` slot — it's the same intended
entry.

The broker rejection is observable as `entry-failed: too-close-to-market`
(vs the generic `entry-failed: broker rejected the order`). The recovery
skip reasons are logged as `recover-entry-<reason>` (e.g.
`recover-entry-limit-wrong-side`, `recover-entry-slippage`). Only
TradeNation has a confirmed `#19-10` today; the OANDA path maps its broker
rejections to the generic case (the resolve-time recovery is
broker-agnostic).

**Arming via `tv-arm` (H&S / iH&S):** pass `--recover-entry
market|limit|abort` to bake the policy onto the `05-enter` intent
(`abort` → `skip`). When the flag is **omitted**, the default is keyed off
`--require-confirmation`: a confirmation-required setup (whose
confirmation lag is exactly what strands the stop) defaults to **`limit`**;
otherwise the default is **drop** (`skip`). M/W is out of scope — it has
no stop `EntrySpec` and is unaffected.

**Bar-based order expiry (`expiry_bars`):** a resting stop/limit order
that never fills otherwise sits until `not_after`. Set `expiry_bars: N`
(1..=5) to cancel it N bars after placement instead — useful for a
breakout-stop whose edge is gone if the break doesn't happen promptly.
Neither broker supports a native per-order expiry (TradeNation orders are
all Good-Till-Cancel; the OANDA path uses GTC too), so the worker enforces
it via its scheduled sweep. See [Using `expiry_bars`](#using-expiry_bars)
below for how to set it and what it needs from the chart.

**Anchored vs absolute prices:** `stop_loss` and `take_profit` accept
either form. Anchored (`{ from: low, offset_pips: -2 }`) is computed
from the trigger candle's OHLC at fire time — TradingView fills in the
anchor when the alert triggers. Absolute (`{ absolute: 1.86236 }`) is a
fixed price set at encode time — useful for chart analysis where you've
drawn SL/TP lines and want them honoured exactly.

**Entry-in-range check:** the worker rejects the trade if the trigger
candle's close falls *outside* the SL..TP range — e.g. a gap past TP
would otherwise fill straight into the take-profit. This is the same
gate that protects the absolute-price flow when the trigger candle
moves past one of your fixed levels.

**SL-vs-spread floor (hard limit):** an entry's stop-loss distance must
be at least **10× the live bid-ask spread**, so a stop is a genuine
market level and not dominated by the cost of crossing the book. Enforced
in two places against the same fixed constant
(`trade_control_core::intent::SL_MIN_SPREAD_MULTIPLE`):

- **At fire time (worker)** — every `enter` samples the live spread
  (`get_quote`) and rejects with **HTTP 422 / `rejected:
  sl-below-10x-spread`** if `sl_distance < 10 × spread`. The response body
  names the offending distances in pips, e.g. `entry blocked: SL <= 10x
  spread: SL distance 8.0 pips; spread = 1.0 pips` (pips are
  `distance / pip_size` using the intent's baked `pip_size`). Like the other
  entry gates this is a non-poisoning reject (the id can refire once the
  spread tightens), and it **fails open** on a quote-fetch error so a
  transient broker hiccup never strands a real entry.
- **At build/arm time (M/W only)** — `trade-control build-trade` and
  `tv-arm` reject a too-tight M/W setup before it's ever signed, using the
  arm-time spread baked into the path geometry. H&S has no build-time SL
  (it anchors to the fire-time signal extreme), so H&S relies on the
  worker gate alone.

The multiple is a server-side constant — it cannot be weakened
per-intent, the same discipline as the `min_r` ≥ 1.0 reward:risk floor.

`id` is the **replay-protection key** — the worker remembers each id it
**successfully fulfilled** until just past `not_after`. Gate rejections
(missing prep, active veto, `allow_entry` script returning false,
cooldown, paused, etc.) and broker failures do **not** consume the id —
the same alert can refire and try again. Successful entries, completed
closes, and accepted state-set actions (prep, veto, pause, news-*,
clear-*, unlock) all consume the id, so byte-identical replays of those
return 409 instead of executing twice. Use a unique id per intended
trade.

**Firing outside the time window is a benign decline, not an error.** A
well-formed, correctly-signed intent that arrives after its `not_after`
(expired) or before its `not_before` (too early) is the *expected*
end-of-life outcome for any scheduled alert that keeps firing past its
intent's lifetime. The worker reports it as **HTTP 200** with
`outcome: declined: intent-expired` (or `declined: intent-too-early`) —
distinct from the **400 `rejected`** it returns for a genuinely malformed
or forged request (bad YAML, bad HMAC sig, unsupported version, malformed
`trade_id`). A `time` plaintext stamp more than 24h from now stays a 400
`rejected` (it smells of replay), not a benign decline. The split lets
timeline/verdict tooling tell a routine stale fire apart from a real
bad-body defect — the same status-code convention as M/W's
`declined: mw-not-armed`, here at the parse/verify gate.

## Conditional entries (preps + vetos)

Some setups want to fire `enter` only after a sequence of prior events.
The classic example is "break-and-close below the trend line, retest from
below, then entry candle." Each event is its own TradingView alert; the
worker stores short-lived named flags per-instrument and the `enter`
intent declares which flags must be set (and which must not).

### Continuous at-entry level vetos (`too-low` / `too-high`)

Beyond the named-flag vetos above, an H&S/IH&S enter also carries
**level-bearing** entry vetos baked at arm time — the pcl-exhausted
(`too-low`) and invalidation (`too-high`) prices. The worker re-checks the
**resolved entry price** against these on every fire and rejects
(`rejected: veto-active (<name>)`, HTTP 412, no order placed) when the entry
is already past the level — **independent of whether any cross-event guard
fired**. This is a *continuous* predicate ("is the entry already too far into
the move?"), not a one-shot cross: it catches a price that **gapped past** the
level or was **already past** it when the plan armed, which the engine's
one-shot cross guard would miss. It restores the legacy behaviour where a
persistent KV `too-low`/`too-high` veto blocked a confirmed enter (Bug #12,
the NZD/CAD −110.53 GBP incident). The named-flag `vetos` list is unchanged
and still gates any externally/guard-set KV veto.

A `prep` intent records that a named step happened, with a TTL:

```yaml
v: 1
action: prep
instrument: EUR_USD
step: break-and-close
ttl_hours: 4
```

### Prep cutoffs (`prep-expire` / `<prep>-expiry` line)

A setup is only valid if the prep lands *in time*. An H&S break-and-close
that arrives 124 bars after the pattern start (max 120 on H1) is too late
— the pattern has grown too big to be a clean H&S, and a trade taken off
it tends to lose. `prep-expire` is the operator-drawn cutoff for that.

A `prep-expire` intent **blocks all future `prep` fires** for one named
step on the instrument:

```yaml
v: 1
action: prep-expire
instrument: EUR_USD
step: break-and-close
ttl_hours: 24
```

Once the block is set (KV flag `prep-blocked:<scope>:<instrument>:<step>`,
TTL-gated), any later `prep` for that step is rejected with a 409, so an
`enter` whose `requires_preps` lists the step can never be satisfied —
the trade silently never opens. **A prep that already fired *before* the
block is untouched**, so a setup that legitimately landed its prep in time
still enters. No broker call; it's pure flag state.

The rejection is logged but does **not** poison the seen-id (same
replay-scope rule as other gate rejections), so a replayed prep just
re-logs. The three log lines — `prep-expire stored`, `prep rejected —
expired`, and the enter gate's `missing-prep` — let you reconstruct the
timeline later.

**On the chart:** draw a vertical line labelled `<prep>-expiry` at the
last bar the prep may land —
`break-and-close-expiry` or `retest-expiry` (aliases `neckline-expiry` /
`retrace-expiry`). `tv-arm` classifies it, resolves the stem to its
canonical prep step (latest line wins per step), and emits one
`08-prep-expire-<step>` alert bound to the line. When price/time crosses
the line, the alert fires and the block lands.

`tv-arm` guards the geometry: a `<prep>-expiry` line in the **future**
whose matching prep trend line is **missing** is a hard error (the setup
could never enter, so arming it is pointless); a line already in the
**past** is a warning (re-arm later in time). `trade-expiry` is *not* a
prep cutoff — it keeps its dedicated whole-trade-close meaning and never
collides with this vocabulary.

A `veto` is the inverse — a named blocker that must be absent for entry
to fire:

```yaml
v: 1
action: veto
instrument: EUR_USD
trade_id: eurusd-hs-1     # required — the setup this veto belongs to
name: news-window
ttl_hours: 6
# level: cancel-pending   # optional; default stop-next-entry
```

**Vetos are scoped per setup, not per instrument.** The KV key is
`veto:<account>:<trade_id>:<instrument>:<name>`, so a veto recorded under
one `trade_id` only blocks entries that carry the **same** `trade_id`. A
`too-high` veto fired during setup A can no longer bleed into a later,
independent setup B on the same pair — the bug that previously stranded a
stale veto in KV (a long `not_after` kept the old key alive past the setup
that set it). `trade_id` is **required** on `enter`, `veto`, and
`clear-veto`; the worker rejects an intent that omits it (HTTP 400). The
`enter` gate looks vetos up by the entry's own `trade_id`, so the veto and
the entry it guards must agree on the value — which they do, because every
alert in a `build-trade` bundle shares one minted `trade_id`.

The optional `level` field escalates a veto beyond a flag-only gate:

- `stop-next-entry` (default) — KV flag only. Blocks any future `enter`
  that lists this name in its `vetos:`.
- `cancel-pending` — also cancels resting stop / limit pending orders
  for the instrument right now. Useful when a setup invalidates while
  you have an entry sitting at the broker (e.g. price retraced past your
  pin-bar low). Open positions are left alone.
- `close-positions` — also closes any open positions for the
  instrument. The strongest level; closest to a per-name `invalidate`,
  except that other strategies can still trade the instrument as long
  as they don't list this veto name.

The flag itself always persists for `ttl_hours`. Broker side effects
are one-shot at fire time, but re-firing a higher-level veto repeats
them (alerts can drop; re-applying is cheap).

`invalidate` is still the right tool for "kill everything on this
instrument right now" — it sets an instrument-wide cooldown that
blocks **all** future entries regardless of any `vetos:` opt-in.

The `enter` intent then opts in:

```yaml
v: 1
action: enter
instrument: EUR_USD
direction: short
entry: { type: market }
stop_loss:   { from: high, offset_pips: 2 }
take_profit: { from: close, offset_r: 2.0 }
risk_pct: 0.5
requires_preps: [break-and-close, retest]
vetos: [news-window]
```

The worker rejects the entry with HTTP 412 if any required prep is
missing, if the preps' stored `set_at` timestamps are not strictly
increasing in list order, or if any opted-in veto is active. Preps are
**not** consumed on entry — they linger until TTL or explicit
`clear-prep`. Re-firing a prep refreshes its timestamp and TTL.

`requires_preps` and `vetos` are template-only fields; the CLI does not
prompt for them. Author one template per setup.

## Break-even stop management

Once a candle **closes** past 50% of the way from entry to take-profit (in the
trade direction), the worker moves the stop-loss to **break-even** — the entry
price exactly — so a leg that ran most of the way to TP and then reverses
scratches at **0R** instead of taking a full **−1R**. This encodes the standing
lesson: *once profit reaches 50%, set SL to break-even.*

- **Default ON at 50%** for every pattern enter armed via `tv-arm` /
  `build-trade` (H&S, the strategy-v2 Quasimodo leg, and M/W).
- **Latched / one-way.** It arms once; the stop is moved to entry and never
  reverts. The broker's resting stop handles everything else.
- **Arming basis is the candle CLOSE**, not an intrabar wick — a fakeout spike
  to the midpoint that closes back does **not** arm it.

### How it works

The rule is baked as a signed `breakeven: { threshold: <f> }` field on the
`05-enter` intent (covered by the whole-body HMAC, so it can't be tampered).
Two consumers honour it identically, sharing one pure helper in
`trade_control_core::intent::Breakeven` so they can't drift:

- **Live worker** — a cron step (`breakeven_watch`) runs every 15-min tick. For
  each open position whose enter carried `breakeven`, it fetches the closed
  candles since the fill at the trade's timeframe and, once one has closed past
  the 50% level, calls `amend_stop(entry)` (a broker-native SL move). Idempotent
  — re-running is a no-op once the stop is at break-even.
- **Offline replay** (`replay-candles`) — `simulate_fill` walks the candle path
  and moves its tracked stop to entry on the same close-past-50% rule; the
  report shows `BREAK-EVEN (SL→BE)` when a position closed at the moved stop.

### How to set it

- **tv-arm:** on by default. `--no-breakeven` opts out; `--breakeven-pct <f>`
  overrides the threshold (e.g. `--breakeven-pct 0.7`).
- **`build-trade` trade spec:** `breakeven_pct: 0.5` (the default). Set
  `breakeven_pct: null` to disable for that trade, or a custom fraction to
  change the threshold. Values outside `(0, 1)` are clamped to 0.5.

> **Demo-confirm before trusting live.** Like the spread-blackout stop widen,
> the break-even move uses `amend_stop` on an **open** position; TradeNation's
> `AmendCloseOrder`-on-open-position path is demo-unverified. Every intended
> amend is logged prominently first so a demo run can read it back (SL moved to
> entry, TP unchanged) before this is trusted on a live account.

## Using `expiry_bars`

`expiry_bars` cancels a resting **entry** order if it hasn't filled
within N bars (1..=5) of being placed, instead of letting it rest until
`not_after`. It's for breakout setups: a `stop` entry is only worth
keeping for a few bars after the signal — if the break hasn't happened by
then, the edge is gone and a late fill is usually a worse trade.

### When to use it

- You're using a **`stop`** (breakout) or **`limit`** (pullback) entry —
  it does nothing for a `market` entry, which fills immediately.
- The order's value decays with time: you want "fill in the next 2–3
  bars or forget it," not "fill any time in the next 12 hours."

### How to set it

Three equivalent ways, depending on where you author the trade:

1. **Directly in an `enter` intent** (hand-authored template):

   ```yaml
   v: 1
   action: enter
   instrument: EUR_USD
   direction: long
   entry: { type: stop, from: high, offset_pips: 2 }
   stop_loss:   { from: low,  offset_pips: -2 }
   take_profit: { from: close, offset_r: 2.0 }
   risk_pct: 0.5
   expiry_bars: 3        # cancel if unfilled 3 bars after placement
   ```

2. **In a `build-trade` trade spec** (`expiry_bars: 3`) — it lands on the
   `05-enter` alert only; vetos and preps never carry it.

3. **From `tv-arm`** when arming a chart: `tv-arm … --expiry-bars 3`.

Omit it entirely to keep the old behaviour (rest until `not_after`).

### What it needs from the chart (important)

The worker can't compute "3 bars from now" itself: a resting order gets
no further alerts to count bars from, and "3 bars after a Friday close"
is *Monday's* session open, not Friday evening — the worker has no
session calendar to know that. **Only the indicator does.** So the Pine
study (`candle-signals-v2.pine` **v2.3+**) ships five hidden plots
`next_candle_timestamp_1..5`, each
`time_close(timeframe.period, bars_back=-k)` — the forward bar-close
times, computed against the symbol's session schedule (weekends and
session breaks skipped). At fire time TradingView fills those into the
alert; the worker reads slot `expiry_bars` and sets the order's
`cancel_at = min(menu[expiry_bars], not_after)`. The scheduled sweep then
cancels the order once `cancel_at` passes (logged `reason=bar-expiry`).

So before using `expiry_bars` live: **republish the v2.3 `Candle Signals`
study to TradingView.** Until you do, the `next_candle_timestamp_*` plots
don't exist and the menu arrives empty — the worker then safely falls
back to `not_after` (no crash, just no tightened expiry). The menu is
only attached to the signed enter body **when `expiry_bars` is set**, so
trades that don't use the feature are byte-identical and don't depend on
the v2.3 plots at all.

### Edge cases

- **Out of range:** `expiry_bars` outside 1..=5 is rejected at fire time
  (`rejected: expiry-bars-out-of-range`, HTTP 400). The rejection does
  **not** consume the id, so the next bar's alert can still get in.
- **Capped at `not_after`:** the expiry never outlives the alert window —
  if N bars would land past `not_after`, `not_after` wins.
- **Non-time charts** (tick / Renko): `time_close` returns `na`, the menu
  slot is empty, and the worker falls back to `not_after`.
- **One-off holidays:** Pine projects against the *regular* session
  schedule, so an unscheduled holiday inside the window can shift the
  timestamp; `not_after` is the backstop.

## CLI

Build:

```sh
cargo build --features cli --release --bin trade-control
```

### Shell completions

The CLIs are installed per-environment with suffixed names
(`trade-control-staging`, `tv-arm-staging`, … and the `-dev` set — see
**Deploy**). Each binary's completion binds to **its own** name (taken from
`argv[0]`), so `trade-control-staging --print-completions` defines
completions for `trade-control-staging`, not the bare `trade-control`. Eval
each from your shell rc:

```sh
# ~/.zshrc — one line per installed binary (absent ones no-op):
eval "$(trade-control-dev --print-completions 2>/dev/null)"
eval "$(trade-control-staging --print-completions 2>/dev/null)"
eval "$(tv-arm-dev --print-completions 2>/dev/null)"
eval "$(tv-arm-staging --print-completions 2>/dev/null)"
# …and tv-news-{dev,staging} likewise.
```

To write a static completion file for an explicit shell instead, use the
`completions <shell>` subcommand:

```sh
trade-control-staging completions zsh > ~/.zfunc/_trade-control-staging
```

Both emit the same script. For zsh it also appends a dynamic completer
that fills the `instrument` positional from the live TradeNation catalog
when `--broker tradenation` is in argv (see the `run_completions` doc
comment for wiring it into `compdef`).

Generate a signing key once, store the same file on your machine and as
the `SIGNING_KEY` wrangler secret:

```sh
./target/release/trade-control gen-key > ~/.config/trade-control/key.hex
wrangler secret put SIGNING_KEY < ~/.config/trade-control/key.hex
```

The key is used as the HMAC-SHA256 secret over the signed body — no
encryption. Intent fields are cleartext on the wire (visible in
TradingView and in Cloudflare's request log).

### Signing an intent

The CLI reads a YAML *template* — typically a partly-filled intent with the
boilerplate (`v: 1`, `action`, SL/TP style) already set — and prompts you for
each missing required field. Keep a couple of templates in `~/.config/trade-control/`,
one per setup style.

Example template `pin-bar-long.yaml`:

```yaml
# Bullish pin-bar entry template — the CLI will prompt for instrument, id, not_after.
v: 1
action: enter
direction: long
entry: { type: market }
stop_loss:   { from: low,  offset_pips: -2 }
take_profit: { from: close, offset_r: 2.0 }
risk_pct: 0.5
```

Run:

```sh
./target/release/trade-control sign \
  --key-file ~/.config/trade-control/key.hex \
  --template pin-bar-long.yaml
```

The CLI prompts for missing fields (`instrument`, `id`, `not_after`), then
emits the cleartext signed alert body on stdout. Paste it directly into
the TradingView alert message. Convenience defaults:

- `not_after`: type a duration like `8h` or `2d` (relative to now), or paste
  an absolute ISO-8601 timestamp.
- `id`: defaults to `<instrument>-<YYYY-MM-DD>-<random>`. Press Enter to accept.

For scripted use, pass `--non-interactive` to make the CLI hard-fail on any
missing field instead of prompting:

```sh
./target/release/trade-control sign \
  --key-file ~/.config/trade-control/key.hex \
  --template fully-specified.yaml \
  --non-interactive
```

To check what arrived on the worker, pass the body back through `verify`
(reads from stdin, a positional, or `--file`):

```sh
curl ... | ./target/release/trade-control verify \
  --key-file ~/.config/trade-control/key.hex
```

### Querying state, unlocking, and managing preps / vetos

Several subcommands talk to the *running* worker, using the same signing
key as auth. The worker URL comes from the binary's **baked-in default**
(each suffixed CLI targets its own environment — `trade-control-staging`
hits the staging worker, no config needed). Precedence is `--endpoint` flag
> `TRADE_CONTROL_ENDPOINT` env > baked default. Don't export
`TRADE_CONTROL_ENDPOINT` globally in your rc — it would override every
suffixed binary's baked URL and point them all at one worker. Use the flag
for ad-hoc overrides:

```sh
# Normal use — the baked default targets the right worker:
trade-control-staging status --key-file ~/.config/trade-control/key.hex

# Ad-hoc override (examples below use --endpoint explicitly):
export TRADE_CONTROL_ENDPOINT=https://trade-control.<account>.workers.dev

# Dump active cooldowns, preps, vetos + recent seen ids as YAML.
./target/release/trade-control status \
  --key-file ~/.config/trade-control/key.hex

# Clear a cooldown set by an `invalidate` you didn't mean to send.
./target/release/trade-control unlock EUR_USD \
  --key-file ~/.config/trade-control/key.hex

# TradeNation trading hours + market details for one instrument. Accepts a
# canonical name (US30, EUR_USD) or the TN MarketName ("Wall Street 30");
# the CLI resolves it via the catalog (use --force to skip), the worker
# resolves it against the broker. `hours` is an alias. TradeNation only.
./target/release/trade-control market-info "Wall Street 30" \
  --key-file ~/.config/trade-control/key.hex
./target/release/trade-control hours US30 \
  --key-file ~/.config/trade-control/key.hex

# Set / clear preps and vetos directly. (TradingView normally fires these,
# but the CLI is the manual escape hatch — e.g. when a prep went stale and
# should be dropped before TTL.)
./target/release/trade-control prep EUR_USD break-and-close --ttl-hours 4 \
  --key-file ~/.config/trade-control/key.hex
# Vetos are scoped per setup, so `veto` / `clear-veto` require --trade-id
# (the setup the veto belongs to). It must match the trade_id the entry
# carries, or the veto won't gate it.
./target/release/trade-control veto EUR_USD news-window \
  --trade-id eurusd-hs-1 --ttl-hours 6 \
  --key-file ~/.config/trade-control/key.hex
# Escalated veto: also cancel resting pending orders for the instrument.
# Add --level close-positions to also close open positions.
./target/release/trade-control veto EUR_USD structure-broken \
  --trade-id eurusd-hs-1 --ttl-hours 4 \
  --level cancel-pending \
  --key-file ~/.config/trade-control/key.hex
./target/release/trade-control clear-prep EUR_USD break-and-close \
  --key-file ~/.config/trade-control/key.hex
# Preps/vetos are keyed by account scope. An account-scoped prep/veto (one
# whose `status` row shows `account: reversals`) is NOT cleared by the
# default (global `_`) clear — pass --account to match the scope it was set
# under, or the clear is a silent no-op:
./target/release/trade-control clear-prep "NZD/JPY" break-and-close \
  --broker tradenation --account reversals \
  --key-file ~/.config/trade-control/key.hex
./target/release/trade-control clear-veto EUR_USD news-window \
  --trade-id eurusd-hs-1 \
  --key-file ~/.config/trade-control/key.hex
# The experimental veto-on-reversal hook writes its veto under the fixed
# name `reversal`. If a reversal-close vetoed a setup you still want to take,
# clear it the same way (the name is always `reversal`):
./target/release/trade-control clear-veto EUR_USD reversal \
  --trade-id eurusd-hs-1 \
  --key-file ~/.config/trade-control/key.hex

# Stranded bad-name entries: `unlock` / `clear-prep` / `clear-veto` normally
# validate the instrument against the TradeNation catalog before sending,
# so a non-canonical name like `XAUUSD.F` is rejected with a candidate list.
# When the worker already holds such a string in KV, pass `--force` to
# skip validation and send the name verbatim — the only way to clear a
# stuck key short of `wrangler kv:key delete`.
./target/release/trade-control clear-veto "XAUUSD.F" too-low \
  --trade-id xauusd-hs-1 --broker tradenation --force \
  --key-file ~/.config/trade-control/key.hex
```

`status` returns:

```yaml
now: "2026-05-14T03:21:00Z"
cooldowns:
  - instrument: EUR_USD
    expires_at: "2026-05-14T15:21:00Z"
recent_seen:
  - id: pin-bar-eurusd-2026-05-13-a
    expires_at: "2026-05-14T02:00:00Z"
preps:
  - instrument: EUR_USD
    step: break-and-close
    set_at: "2026-05-14T02:30:00Z"
    expires_at: "2026-05-14T06:30:00Z"
vetos:
  - trade_id: eurusd-hs-1
    instrument: EUR_USD
    name: news-window
    expires_at: "2026-05-14T09:00:00Z"
# instruments:
#   - EUR_USD → EUR/USD
#   - XAUUSD.F (no TN catalog match)
```

The trailing `# instruments:` block annotates each unique instrument
string in the snapshot. `→ Canonical Name` tells you what to type for
`clear-veto` / `clear-prep` / `unlock` (the TradeNation catalog often
holds the same FX pair under a slash-form name). `(no TN catalog
match)` flags strings the catalog can't resolve — typically OANDA-only
exotics, or stranded non-canonical names that need `--force` to clear.
The block is best-effort: if the TradeNation login or catalog read
fails the names are listed without annotations, and the block is
omitted entirely when the snapshot has no instrument fields.

`unlock` returns:

```yaml
unlocked: EUR_USD
was_cooled_down: true
```

`market-info` pretty-prints the broker's market details, **Brisbane time
first** (the operator's zone) with London alongside:

```
Wall Street 30

trading hours (Brisbane / London):
  09:00 (+1d) - 07:00 (+1d)   (London 23:00 - 21:00)

spread:            4
margin:            0.5%
stop orders:       Yes
guaranteed stop:   Yes (distance 2%, charge 3)
min/max stake:     USD,0.1,1000000
contract:          Rolling (rolling: true)
expiry:            - (London -)
```

Trading hours come from the broker in **London local**; the Brisbane
(UTC+10) equivalents are shown first. A `(+1d)` suffix on a Brisbane time
means it falls on the next calendar day. When the broker returns
non-range text (e.g. `24 Hours`), that raw string is printed verbatim
instead of a parsed range. The worker itself returns the raw `MarketInfo`
as YAML (machine-friendly); the Brisbane-first layout is the CLI's doing.

All control subcommands use the same replay-protection mechanism as the
trade actions — re-running the same `unlock` (or `clear-prep`, etc.)
within its window won't double-fire.

## Secrets

| Name | Required | Notes |
|---|---|---|
| `SIGNING_KEY` | yes | 64-hex-char HMAC-SHA256 key. Used to sign / verify the body and (re-used) to gate `GET /diag/*`. |
| `OANDA_API_KEY` | for OANDA | OANDA v20 token. |
| `OANDA_ACCOUNT_ID` | for OANDA | OANDA account id, for the legacy global path (intents with no `account:`). Named accounts carry their id on metadata instead. |
| `OANDA_LIVE` | no | Practice vs live for the **legacy global path only** (`account: None`). `true` → live, anything else / absent → practice. **Named accounts ignore this** — each account's practice-vs-live host is derived from its own `kind` (`demo` → practice, `live` → live), so one worker can run a demo and a live OANDA account side by side. |
| `TN_ACCOUNT_<NAME>` | for TradeNation | Per-account credentials blob (JSON-serialised `Credentials::TradeNation`). `<NAME>` is the operator-friendly account name uppercased with `-` → `_`. Managed via `trade-control account add` — set this secret per account, the worker logs in on demand and caches the session in KV. See "TradeNation session" below. |
| `MAX_RISK_PCT_PER_TRADE` | no | Hard cap on requested `risk_pct`. Default `1.0`. |
| `MAX_OPEN_POSITIONS` | no | Max concurrent open positions. Default `3`. |
| `PIP_SIZE_<INSTRUMENT>` | no | Override / fallback pip size, e.g. `PIP_SIZE_USD_JPY=0.01`. Used only when the enter intent carries no baked `pip_size` (intents armed through `tv-arm` always do). Default `0.0001`. |

The previous `ENCRYPTION_KEY` and `AUTH_TOKEN` secrets are no longer
used — `SIGNING_KEY` replaces both. After deploying, run
`wrangler secret delete ENCRYPTION_KEY` and (if it was ever set)
`wrangler secret delete AUTH_TOKEN`. Copy the value of your existing
`key.hex` into the new secret: `wrangler secret put SIGNING_KEY <
~/.config/trade-control/key.hex` — the byte format is identical, only
the name and the algorithm using it have changed.

## Request recording (R2)

Every inbound **intent** request (the signed POST path — not `GET /diag/*`
or `/admin/*`) is captured as a single JSON object in R2: the verbatim
signed body, the request headers, the final HTTP status + outcome, and
**every** log line the handler emitted while processing it. The object is
written asynchronously (`ctx.wait_until`) so recording adds no latency to
the response the way back to TradingView.

This is the authoritative archive the tax-tracker / timeline tools consume
— it removes the old need to *reconstruct* the message body from the TV
alert template (which fails once the alert is deleted).

**Object layout:** `req/<YYYY-MM-DD>/<ts>-<request_id>.json`. A day's
requests list under one date prefix and sort by time; filter the records'
`trade_id` field to gather one setup's fires.

**Capture mechanism.** The worker's logging goes through `rlog!` /
`rlog_err!` (record-aware replacements for `console_log!` /
`console_error!`) plus broker-crate `tracing::warn!`/`error!` events — all
tee'd into a per-request buffer that lands in the record. `wrangler tail`
still shows the same console output.

**Fail-soft.** A missing bucket binding, a serialization error, or a
failed R2 put are logged and swallowed — recording never blocks or fails a
trade. If the binding is absent the worker logs one
`recording: no TRADE_CONTROL_R2 bucket bound — skipped` line and carries on.

**Binding.** Requires an R2 bucket bound as `TRADE_CONTROL_R2` in
`wrangler.toml` (`[[r2_buckets]]`), the bucket created
(`wrangler r2 bucket create …`), and the deploy API token to hold
*Workers R2 Storage: Edit*. See the deploy notes for the exact steps.

**Retention.** R2 bundles are **no-TTL** — they persist until a purge command
removes them. Wipe one trade's `ticks/` with `plan purge <trade_id>`, or run a
bulk date sweep over `req/` + `ticks/` with `purge --older-than <days>` (see the
KV namespace "Row lifetimes" note and the CLI section).

### Engine tick-bundles (`ticks/` prefix)

The cron engine (`src/cron/engine.rs`) — not an inbound HTTP alert — is where
every trading decision now happens once a trade runs as a registered
`TradePlan`. So each engine **tick** records a self-contained, replayable
**tick-bundle** to the *same* R2 bucket under a **distinct `ticks/` prefix**, so
the `req/`-reader never trips on it:

**Object layout:** `ticks/<YYYY-MM-DD>/<tick_ts>-<trade_id>.json`. One object per
**noteworthy** `(tick, plan)` that evaluated; a single trade's whole life globs
under its `trade_id`.

Each bundle carries the pure replay tuple `evaluate_plan` consumed — the
`plan`, the prior `PlanState`, the `new_candles` + detector back-window, and the
tick `now`/`expires_at` — plus the golden `PlanEval` output (`fired` /
`new_state` / `done`), the per-fire dispatch outcomes, and the plan-state
`KvTickTransition` (before/after/success/error). Same fire-and-forget
`wait_until` + fail-soft contract as request recording; same `TRADE_CONTROL_R2`
binding. Both shadow (observe-only) and live ticks are recorded; a live tick's
`dispatch_outcomes` carry each fire's broker result, while a shadow tick's is
empty (it dispatches nothing).

**No-op ticks are trimmed.** A tick that saw a new closed bar but where nothing
fired, no phase/state advanced, and the plan isn't done (a "no-op" — common
during an H&S plan's quiet wait for break-and-close) is **not** recorded: its
fat bundle would re-store the whole plan, both states, and the wide detector
window for zero new information. The engine instead emits a single heartbeat log
line (`cron engine: plan <id> tick <now> no-op (…) — not recorded`) so the tick
is still traceable and a gap in the `ticks/` stream is never mistaken for a
stalled cron. The decision is the pure `PlanEval::is_noteworthy(&prior)` (a tick
is noteworthy if it fired, finished, or advanced the FSM's meaningful state —
ignoring the always-moving `watermark` / `expires_at` / `last_close`). KV state
is still persisted every tick regardless; only the *recording* is trimmed.
A **failed** plan-state transition is always recorded (it carries `success:false`
a replay needs).

**Replaying a bundle.** Replay lives in the **`trade-analyzer`** CLI (in the
`trading-tax-tracker` repo), *not* in `trade-control` — that's the downstream
R2-recording consumer, and keeping replay there gives it a single home next to
the bundle/timeline tooling. It replays one bundle offline, either from a local
file or fetched straight from R2 by object key:

```sh
trade-analyzer replay <bundle.json>                       # local file
trade-analyzer replay --from-r2 ticks/<date>/<...>.json   # fetch from R2
trade-analyzer replay --simulate <bundle.json>            # + simulate each enter's fill/exit
```

`replay` re-runs the *same* `evaluate_plan` on the recorded inputs and diffs the
fresh `fired` / `new_state` / `done` against the recorded `eval` — a recorded
tick becomes a deterministic regression test (non-zero exit on any mismatch, so
it gates in CI). `--simulate` additionally resolves each fired enter's
entry/SL/TP (via the pure resolver) and walks the bundle's candle path through a
dumb broker-simulator, reporting filled / stopped-out / took-profit /
never-filled. The pure-evaluation diff validates the *decision* logic; the
simulator the *price-path* — neither runs the worker's broker-dispatch glue
(sizing, seen-id, gates), which is a later step.

### Candle replay (`replay-candles`) + golden fixtures

`replay-candles` (`cli/src/bin/replay_candles.rs`) is the *other* offline
replay: instead of re-running one recorded engine tick, it drives a whole
`TradePlan` over a historical candle **window**, one closed bar at a time —
exactly as the live cron would — and simulates each fired enter's fill. It pulls
the candles from the broker (via candle-cache) and needs no `wrangler dev`, no
HTTP, no live orders. With no window flags it self-resolves the window from the
plan + the live TradingView chart (see the module header); fully-flagged, it
needs no MCP:

```sh
replay-candles --plan plan.json --instrument gbp/aud \
  --granularity 1h --start 2026-06-19T00:00 --end 2026-06-19T15:00
```

The fill simulator is pure and per-enter (entry / SL / TP only), so a
**`06-close-on-reversal`** fire — a separate guard the engine fires when an
opposing reversal candle prints in an `sr_bands` band — is applied as a
post-pass: an open position is flattened on the earliest reversal-close that
lands while it's open, before its SL/TP. The report prints `fill: CLOSED ON
REVERSAL — in @ <entry> → exit <price> (<bar>)` for that enter and tallies it
under `REV:` (distinct from `TP:` / `SL:`). This matches the live worker, whose
`run_close` flattens the broker position on the same fire; before the fix the
close was inert and the position over-held to SL/TP/window-end.

**Seeing the engine's silent state changes (`--verbose` / `--all-events`).**
The normal report lists only *fires* — intents the engine emits. But the engine
also advances state per bar that fires nothing: the spine phase
(`AwaitBreakAndClose → AwaitEntry → Done`), the break-and-close stamp, and —
most confusingly — the **retest stamp**. Retest is *not* an emitted prep; it's a
retroactive `retest_seen_at` lookback that the entry gate reads. So a plan whose
`requires_preps` lists `retest` will never show a "retest" fire, which reads like
the step was skipped when it wasn't. `--verbose` (alias `--all-events`) prints a
bar-by-bar trace of these state moves *before* the fire report, showing exactly
which bar stamped the retest and when the spine advanced (quiet bars are
omitted):

```sh
replay-candles --plan plan.json --verbose
```

```text
Bar-by-bar engine trace (--verbose):
  bar 2026-06-23 15:00:00 +10:00 phase=AwaitEntry
    phase AwaitBreakAndClose→AwaitEntry
    ✓ break-and-close stamped (spine → AwaitEntry)
    → fired 03-prep-break-and-close
  bar 2026-06-23 17:00:00 +10:00 phase=AwaitEntry
    ✓ retest stamped (entry gate now satisfied)
  bar 2026-06-23 18:00:00 +10:00 phase=AwaitEntry
    → fired 05-enter
```

It's a pure diff of the `PlanState` before/after each tick — no engine change,
no extra evaluation — so it's always safe to add to any replay.

**Break-even arming (`be:` line).** Break-even is *not* a fill-time decision —
in production the live cron (`breakeven_watch`) sends `amend_stop(entry)` to the
broker on the first 15-min tick that observes a candle closing past the
threshold (50%-to-TP by default). So a trade reported as `BREAK-EVEN (SL→BE)`
looks like it stopped at entry for no visible reason. The per-enter section now
shows a `be:` line — printed whenever break-even arms during a trade's life (any
outcome) — naming the **bar whose close armed it**, i.e. when the live worker
would have amended the broker SL:

```text
• 05-enter Enter @ 2026-06-23 23:00:00 +10:00  close=5.924
    order: SHORT stop @ 5.9168  SL 5.9562  TP 5.7657
    be: SL→break-even @ 2026-06-24 12:00:00 +10:00 (a candle closed past 50%-to-TP; live cron amends the broker SL here)
    fill: BREAK-EVEN (SL→BE) — in @ 5.9168 → SL 5.9168
```

This is computed by the pure `breakeven_armed_at` helper (engine), which shares
its fill-finding and arming predicate (`Breakeven::close_arms`) with
`simulate_fill`, so the reported arming bar can't drift from the simulated
break-even outcome. It does **not** change `SimOutcome` (so saved fixtures are
untouched). The live worker's break-even path is unchanged — this is replay
reporting only.

**Why an order "NEVER FILLED" — the sweep reason.** A resting stop/limit order
that never triggers isn't necessarily one the live worker passively let sit:
every 15-min cron tick the worker's order **sweep** (`src/cron/sweep.rs`)
cancels a still-pending order once its alert window expired, its bar-based
`cancel_at` (`expiry_bars`) passed, it sits inside a market-hours blackout, or
current price overtook its stop-loss. So a plain "never filled" hides whether
the worker would have *actively swept* the order. The `NEVER FILLED` line now
names the sweep reason when there is one:

```text
    fill: NEVER FILLED — swept: SL breached @ 2026-06-19 12:00:00 +10:00 (live cron cancels the resting order here)
    fill: NEVER FILLED — swept: bar-expiry @ 2026-06-19 13:00:00 +10:00
    fill: NEVER FILLED — alert-window expired @ 2026-06-19 14:00:00 +10:00
```

When no sweep condition is reached it keeps the original wording, `fill: NEVER
FILLED (pending order untriggered in window)`. The decision is the pure
`sweep_reason` helper (engine), which reuses the shared `core::sweep_gate`
predicates (`breach_detected` / `bar_expiry_due` / `market_blackout_due`) the
live worker's sweep uses — so worker and replay can't drift. Like
`breakeven_armed_at` it does **not** change `SimOutcome`, so saved fixtures are
untouched. (The market-hours **blackout** sweep is not reconstructed offline —
the per-instrument no-entry windows live in KV — so a blackout-driven cancel
still shows the plain wording; surfacing it is REPLAY-PARITY-AUDIT item 3.)

**Freezing a known-good run as a regression fixture.** When a replay is
producing the verdict you want, add `--save <name>` to freeze that run into
`replay-fixtures/<name>/` — four JSON files: the `plan`, the **exact candle
window** (`candles.json`, so the fixture is offline forever — broker history can
change, the disk cache can be wiped, the fixture still runs identically), the
resolved `meta` (instrument / granularity / source / window), and the golden
`expected.json` outcome (each fire's decision + simulated fill, the terminal
phase, warnings):

```sh
replay-candles --plan plan.json --instrument gbp/aud \
  --start 2026-06-19T00:00 --end 2026-06-19T15:00 --save gbpaud-expiry-2026-06-19
```

**Re-running a fixture offline.** `--test-mode --fixture <name>` loads the
frozen plan + candles + meta and replays them with **no network, no env vars,
no TradingView** — the candles come from `candles.json`, the granularity/window
from `meta.json`. Add `--check` to diff the fresh outcome against
`expected.json` and exit non-zero on any mismatch (the gate proof):

```sh
replay-candles --test-mode --fixture gbpaud-expiry-2026-06-19 --check
```

**The regression suite.** A `#[test]` (`all_fixtures_match_expected`) globs
`replay-fixtures/*/` and re-runs every saved fixture through the pure engine on
`cargo test` — so every deploy's test gate re-verifies all known-good scenarios.
A later engine change (a cross-mode tweak, a resolver edit, a veto-gate fix) that
silently moves a verified verdict fails here. The expected snapshot is
**structured JSON of outcomes**, not the human report text, so cosmetic report
changes don't churn fixtures; after an *intentional* outcome change, re-save the
fixture. The snapshot freezes today's mid-only fill semantics on purpose — a
change to the simulator's fill model will (correctly) flag.

**Replay enforces the news blackout (pause/resume), not just logs it.** A plan's
`pause`/`resume` control rules fire on their `TimeReached` epochs; the replay
applies them to its in-memory state store and **gates `05-enter` on them** —
exactly as the live worker's `run_enter` blackout gate does. An enter that fires
inside an active pause is a **hard skip** (NO FILL / 0R), shown in the report as
`fill: SUPPRESSED — trade paused by news blackout [...] → NO FILL / 0R` and not
tallied as a win. The decision lives once in `trade_control_core::pause_gate`
(`apply_pause` / `apply_resume` / `entry_blocked`), shared by the worker and the
replay so they can't drift. Because the replay runs over *historical* bars, its
`MemStateStore` clock is pinned to each tick's time (`set_clock`) — otherwise a
pause stamped at, say, 2026-05-29 would be judged expired against today's
wall-clock and vanish (the same wall-clock-vs-cursor trap as the tv-arm prune).
Without a blackout (or with `--skip-calendar-bars`) the same enter fills — that
with/without pair is the A/B the journal needs to price what the news rule cost.

**Replay enforces the spread-blackout reject (System 1), not just the fill.**
The live worker rejects a new entry that fires during the post-NY-close
liquidity trough when the instrument's spread is elevated (see *Spread-blackout
window* above). The replay now mirrors it: `simulate_fill` computes the **fire
bar**'s spread from the recorded bid/ask book (`(ask_c − bid_c) / pip_size`) and
calls the *same* `trade_control_core::spread_blackout` decision + per-instrument
threshold the worker uses. On a reject it returns the new
`SimOutcome::SpreadBlackout { spread_pips, threshold_pips }`, shown in the report
as `spread: REJECTED — spread 30.0p > 8.0p threshold inside the NY-close-edge
window (no order placed; live worker 423s)` and not tallied as a win. A
mid-only feed (`bid == ask`) has zero spread → never blacks out (we don't
fabricate a spread the data doesn't carry). **Modelling note:** the live gate
only samples when the KV `spread-blackout:window` marker is set (the daily cron
writes it at the NY-close edge); the replay has no KV, so it approximates the
marker with `core::ny_clock::is_ny_close_edge(fire_bar.time)` — exactly the
close-edge hour, where the live window can persist a little longer until the
recovery watcher clears it.

**News / blackout pruning is replay-cursor-aware.** When `tv-arm` builds a plan
it fetches the week's forex-factory events and adds one blackout pair + one news
pair per event, then drops any pair whose window has already elapsed. The
"as-of" time it prunes against depends on the run mode:

- **`--register-plan` (live arm):** wall-clock now — a genuinely stale event is
  still dropped before it reaches the live worker.
- **`--plan-out` (offline / replay build):** the chart's **replay cursor** —
  `bars_range.to`, the last *loaded* bar, **not** the visible-window right edge
  (on a rewound chart the visible window still extends past the last bar into
  empty future space, so `visible_range.to` overshoots the cursor and would
  prune events that are genuinely upcoming relative to it). Clamped to now. So
  arming off a rewound chart keeps a blackout that is still *upcoming relative
  to the cursor* even though it's in the past relative to today — the replay can
  then reproduce a news skip the live system actually made. (Before this fix the
  prune always used wall-clock now, so every historical replay silently ran with
  blackouts removed.) `--as-of <RFC3339>` forces an explicit cursor for headless
  / cron replays where no live chart range is readable. The drop log line records
  the `as_of=` it used and its `source` (`wallclock` / `replay-cursor` /
  `as-of-flag`). The **same as-of** flows into the pause/news/calendar-bars
  builders so a pair that survives the prune isn't then rejected by their own
  "refusing to arm a stale blackout" past-window guard.

**Zero-length blackout/news pairs are dropped, not fatal.** When `tv-arm`
auto-draws calendar lines and reads them back, TradingView snaps each vertical
line to its bar's timestamp — so two distinct planned times that fall in the same
bar (e.g. one event's `resume` and the next event's `pause` 8h apart on an H1+
chart) come back on the *same* timestamp. The readback pairing
(`trading-view/src/pair_lines.rs`) zips sorted starts to sorted ends and now
**drops any pair whose start and end snapped to the same time** — a zero-length
window arms nothing — instead of hard-erroring `blackout pair is reversed` and
aborting the whole arm. A genuinely reversed pair (`start > end`, drawn out of
order) is still a hard error.

## Brokers

The intent YAML carries an optional `broker:` field, one of `oanda`
(default) or `tradenation`. Each broker is independent — the operator
picks per intent at sign time:

```yaml
v: 1
action: enter
broker: tradenation       # or omit for OANDA
instrument: EUR/USD
# ...
```

### Broker trait surface (contributor note)

Each broker crate implements the `Broker` trait in `core/src/broker.rs`.
Alongside the entry / close / cancel / lookup actions it exposes
`get_quote` (live bid/ask → `Quote { bid, ask }`, with `mid()` and
`spread()`), `list_open_positions`, `amend_stop` (move a stop-loss,
leaving TP / trigger / stake untouched), and `list_pending_orders`.
`get_current_price` is a default method = `get_quote().mid()`. These are
**foundations for the spread-blackout feature and carry no operator-visible
behaviour** — no worker action calls them yet. TradeNation implements all
four; OANDA implements all four via its v20 trade/order/pricing endpoints.
**Caveat (TradeNation `amend_stop`):** the upstream `AmendCloseOrder`
endpoint is unverified against an *open position's* SL — a later sub-plan
must demo-confirm it before any live stop-widening.

### Spread-blackout window

Right after New York's 17:00 close there is a ~1-hour global liquidity
trough where the broker's spread on thin FX crosses (EUR/NZD, AUD/NZD)
blows out and snaps back at the next hour. The dangerous hour tracks
**New York's clock** (DST-aware): 07:00 BNE under EDT, 08:00 BNE under
EST. The state machine + cron skeleton arms a global window marker at
that edge; **System 1** rejects *new* entries during the window;
**System 2** (below) widens *already-open* positions' stops away from
price and restores them after; **System 3** (below) cancels *resting entry
orders* during the window and re-drives them after.

> **Status (2026-06-13): all four pieces shipped, in demo-validation.**
> The state machine + both crons + Systems 1/2/3 are coded, unit-tested,
> and on `main` (tags `v17`–`v22`). The build is green on native + wasm +
> cli. It is **NOT yet proven live** — a week of demo testing on the
> `reversals` TradeNation account is in progress. Two things **must** be
> confirmed on demo before any live use, and the thresholds **must** be
> calibrated against real trough data:
>
> 1. **`amend_stop` on an OPEN position works** (System 2). The upstream TN
>    `AmendCloseOrder` had zero prior callers; it is unconfirmed whether it
>    amends an *open position's* SL or only a resting order's. Until a demo
>    confirms the read-back, **live stop-widening must stay off.** The apply
>    cron logs an `INTENT amend_stop …` line before every amend precisely so
>    a dry-run/demo can confirm without risk.
> 2. **cancel + re-drive of a resting order works** (System 3), including the
>    `recover_entry` recovery when the level has been overrun. Re-drive
>    re-runs the real HMAC verify on the stored signed body (no fabricated
>    auth) — confirm a cancelled order actually re-places (or correctly
>    drops) on demo.
> 3. **Some thresholds are still provisional**: System 1's elevated cutoff
>    is now per-instrument and baked from sampled spreads (see below), but
>    `SPREAD_BLACKOUT_RECOVERED_PIPS` (4p, System 2/3 restore) and the
>    `clamp_widen` floor/ceiling (22p / 40p) are still flat placeholders.
>    Tune against observed trough spreads during the demo week, and re-bake
>    the per-instrument table as the sample window lengthens.
>
> See the **Demo-validation checklist** in `TODO.md` for the step-by-step.

#### System 1 — reject new entries during the window

When the global window is open, a brand-new `enter` is checked at the
**very end** of entry processing — after every gate
(retry/cooldown/prep/veto/`allow_entry`) and geometry resolution have
passed, immediately before the broker order. The worker samples the
**live spread** (`ask − bid` via `Broker::get_quote`) for the incoming
instrument and, if it exceeds the elevated cutoff (in pips), rejects:

- **Outcome:** `rejected: spread-blackout`, **HTTP 423 Locked** (mirrors
  the pause / cooldown / news state-block family — the intent is valid,
  the condition is transient, a later fire can succeed).
- **No instrument classification.** The spread *sample itself* is the
  filter: a major (EUR/USD ~1p) firing during the window passes; a thin
  cross blown out to ~20p is rejected. A day where the spread stays fine
  is not blacked out at all.
- **Reject, NOT delay.** Nothing is persisted, no re-fire is queued, no
  KV is touched. The next legitimate signal bar re-triggers the alert and
  re-runs the check — by then the spread may have recovered and it passes.
- **Does NOT consume the intent id.** Like every `Rejected`, this is a
  `Skip` in the replay-dedup path (no `mark_seen`), so the next fire is
  allowed through (see "Replay protection scope" in `CLAUDE.md`).
- **Fail-open on errors.** A transient window-marker read error *or* a
  `get_quote` error at decision time logs a `console_error!` and **allows**
  the entry — a transient hiccup must never block a legitimate trade. (A
  fail-closed variant is an open question; see `src/spread_blackout.rs`.)
- **Window closed = zero cost.** When the marker is absent the worker
  falls through without any broker round-trip (no `get_quote` call).

The elevated cutoff is now **per-instrument, baked at compile time** from
real sampled spreads (it was a flat 8-pip constant, which mis-fired badly
for non-FX: Copper's *normal* spread is ~150 pips, so the flat 8 blocked
every legitimate Copper entry during the window). `core/build.rs` reads the
committed YAML samples from the **`spread-sampler-cron`** submodule and
emits a per-instrument table (`(name, low, high, median)` in pips, keyed by
the broker-canonical TradeNation name — the same `resolved.instrument` the
gate passes). `elevated_threshold_pips(instrument)` returns
`median × SPREAD_REJECT_MULTIPLE` (**5× the instrument's own normal
spread**). The 2026-06-23 spread-hour data showed the post-NY-close blowout
is an **FX** phenomenon — FX crosses spike 10–20× their normal (AUD/USD
0.4p→6p, EUR/GBP 0.5p→10p) while commodities/indices (Copper, Gold) stay
flat — so a multiple of each instrument's *normal* is the right shape: 5×
sits above resting/busy-news jitter yet well below a ≥10× spread-hour
spike, so it rejects the blowout (AUD/USD line = 2p) without ever
false-blocking a flat-spread instrument (Copper normal ~150p → line 750p).
An instrument absent from the baseline (a fresh asset, or one with no pip
size) falls back to the flat `SPREAD_BLACKOUT_ELEVATED_PIPS` (8 pips). The
reject message names the instrument's baked normal/seen-range and the
current spread. The recovery cutoff (`SPREAD_BLACKOUT_RECOVERED_PIPS`,
4 pips) is still a flat constant and lives beside the elevated logic. The
whole feature works in **pips** consistently.

The **pure decision, the per-instrument threshold lookup, the baked
baseline, and the `build.rs` that bakes it now live in
`trade_control_core::spread_blackout`** (not the worker crate), so the
offline candle replay — which links `core` but not the worker `cdylib` —
applies the *same* reject the live worker does
(`[[strategy_changes_in_both_replayer_and_worker]]`). The worker keeps only
the I/O wrapper around the pure decision: the KV `spread-blackout:window`
read + the live `get_quote` sample (`run_enter`) and the recovery watcher
(`src/cron/blackout_watch.rs`), reaching the shared items through the thin
`src/spread_blackout.rs` re-export shim. See *Candle replay* below for how
the replay reconstructs the spread from the recorded bid/ask book.

> **Re-bake cadence:** the table is only as good as the committed samples.
> Re-running `git pull` in the submodule (the hourly cron commits new
> sweeps) and rebuilding picks up fresh data — `cargo:rerun-if-changed` on
> the samples dir forces a rebuild when they grow. Early bakes (a few days
> of data) are conservative on purpose; tighten as the sample window
> lengthens.

#### System 2 — widen open stops during the window, restore after

Right after the NY-close edge, the daily cron also protects every
**already-open** position from the spread blowout: it **widens the
stop-loss away from price** so spread noise can't clip it, then the 15-min
recovery watcher **restores the stop to its exact original level** once the
spread normalises (or a ~3h backstop fires).

- **Direction (away from price).** A **short**'s stop sits above entry, so
  widening moves it **UP**; a **long**'s sits below, so widening moves it
  **DOWN**. (Widening the wrong way would tighten into the spread and clip
  the position instantly — the pure `widened_stop` helper + its
  direction-matrix test guard the sign.)
- **Amount.** Widen by the **live sampled spread in pips**, floored at
  **22p** (the observed EUR/NZD blowout — don't under-widen on a brief
  snap-back) and capped at **40p** (a freak print mustn't blow the stop out
  absurdly). `clamp_widen(live_spread_pips)`.
- **Restore from the remembered original, never `current − widen`.** The
  pre-widen SL is captured into the per-trade record's `original_stops` at
  apply time; recovery amends straight back to that verbatim. This is a hard
  rule: a partial widen, a missed watcher tick, or a double-fire all stay
  correct because the remembered original is idempotent.
- **Bounded extra loss.** Widening temporarily enlarges the *designed* loss
  by **≤ one spread-width** (capped at 40 pips) for **≤ ~1h** (the
  backstop). If a genuine price move runs *through* the widened band during
  the window, the position closes further from entry than its original stop
  — you eat those extra pips. This is the **deliberate, bounded cost**,
  accepted by the operator: the alternative (the original tight stop) is the
  near-certain spread-clip that motivated the feature. It is mitigated
  structurally — the window is driven by the NY-close edge (not a fixed
  Brisbane HH:MM) and Cron 2 restores the moment the spread normalises.
- **Move-only, never close or tighten.** System 2 only ever *moves a stop
  away* then *back*. No code path here closes a position or tightens a stop
  (the same StopNextEntry-only spirit as `veto_on_reversal`).
- **Idempotent.** A re-fired Cron 1 (CF double-deliver / mid-window restart)
  checks the per-trade record's `applied` flag and skips an already-widened
  trade — it never double-widens, and never re-captures the
  already-widened SL as the "original".
- **Crash-safe ordering.** The original is recorded to KV **before** the
  broker amend, so a crash between them can't strand a widened stop with no
  remembered original (the worst case is a restore that's a harmless no-op).

> **PRECONDITION — demo-confirm `amend_stop` on an OPEN position first.**
> TradeNation's `AmendCloseOrder` has zero existing callers and it is
> **UNVERIFIED** whether it moves an *open position's* SL (vs only a resting
> order's). System 2 depends on it. Before trusting the widen live: open a
> demo position on `reversals` with a known SL, `amend_stop` it, read it
> back, confirm the SL moved and the TP is unchanged. The apply cron logs an
> `INTENT amend_stop …` line before every amend precisely so a dry-run/demo
> can confirm the read-back. **Do not enable live widening until this is
> demo-confirmed.** See `TODO.md`.

#### System 3 — cancel resting entry orders during the window, restore after

A resting **stop- or limit-entry order** that sits through the trough can
fill *into* the spread blowout and stop out instantly (the EUR/NZD trade
that motivated the whole feature). So right after the NY-close edge — on the
same affected-account scan as the widen — the cron also **cancels every
resting entry order whose instrument spread is actually elevated** and
stores the order's whole **signed alert body** so the recovery watcher can
**re-drive the exact same entry** once the spread normalises.

- **Only elevated-spread orders are cancelled.** Each found order's
  instrument is spread-sampled via `get_quote`; an order on a still-tight
  major (≤ the elevated cutoff, ~8p) is **left resting**. No
  instrument-classification — the live spread is the filter.
- **Re-drive, don't re-place.** On recovery the watcher reconstructs an
  authentic verified intent from the stored signed body (re-running the same
  HMAC verify the HTTP path does) and calls the **same entry path**
  (`run_enter`) the original alert took — so sizing at the live fill
  reference, the prep/veto/cooldown/allow_entry gates, **and** the
  `recover_entry` stop recovery all apply, with no duplicated place logic.
- **Fill-side recreate geometry (the sign-bug-prone seam).** Before
  re-driving, a pure predicate checks whether the order is still worth
  placing, using **fill-side** bid/ask (a long buys at `ask`, a short sells
  at `bid` — spread counts *against* re-entering a deep order):
  - **Stop still placeable** (fill-side hasn't blown past trigger beyond the
    SL band) → re-drive as a stop.
  - **Stop overrun** (the move is gone) → route to the order's `recover_entry`
    fallback (market / limit / skip) via the broker's own `#19-10` rejection.
    If `recover_entry` is `skip` (the default), it's dropped without a
    pointless broker round-trip and the next signal bar can retry.
  - **Limit still on the pullback side** (fill-side strictly between entry
    and TP) → re-drive as a limit.
  - **Limit stale** (wrong side / past TP) → **dropped**, leaving the trade
    "looking for entry". A limit is itself a fallback, so a stale one is fine
    to drop; it is **never** routed to the stop `recover_entry` path.
- **Crash-safe ordering.** The `CancelledOrder` (signed body + order id) is
  stored on the per-trade record **before** the broker `cancel_order`, so a
  crash between them can't lose a wanted entry — the worst case is a
  recoverable duplicate (a re-drive of an order that never actually
  cancelled), which the re-drive's own gates bound. An order with **no**
  stored signed body is **never cancelled** (we won't strand an entry we
  can't put back).
- **Re-drive ≠ multi-shot re-entry.** A restored order is the *same*
  intended entry, not a re-entry after a stop-out. It's off the HTTP
  seen-id/replay path entirely (the cron calls `run_enter` directly and never
  `mark_seen`s), so a prior successful placement's seen-id doesn't 409 it.
  For single-shot orders (the common resting-order case) it consumes no
  `max_retries` slot. (Multi-shot restore can still burn a slot — an open
  follow-up; see `TODO.md`.)
- **New entry-path KV write.** Every successful single-shot placement now
  also writes an `order:<broker_order_id>` KV row holding the raw signed
  body, TTL'd to the alert window (`not_after` + grace). This is the only
  place the original signed bytes survive long enough for the apply cron to
  find them. It's small (~1KB) and ages out with the order's `EntryAttempt`.

> **PRECONDITION — demo-confirm the cancel + re-drive on `reversals` first.**
> Like the widen, the resting-order cancel/restore is **UNVERIFIED live**.
> Demo protocol (dry-run → demo): place a resting stop-entry before the edge,
> force Cron 1, confirm it's cancelled at the broker and stored in KV
> (`trade-control status` shows the `cancelled_orders` entry); then force
> recovery and confirm it's re-placed (price still on the entry side),
> routed to `recover_entry` (price overran), or dropped (stale limit). **Do
> not enable live until demo-confirmed.** See `TODO.md`.

This release lands the **state machine + cron skeleton** plus Systems 1, 2,
and 3 — the full cancel/restore half. All three systems (reject new entries,
widen open stops, cancel + restore resting orders) are now in place.

Two kinds of KV state live under the `spread-blackout:` namespace:

- **Global window marker** `spread-blackout:window` — `{ opened_at,
  expires_at }`, ~3h TTL. Written by the daily NY-close-edge cron when
  `is_ny_close_edge(now)` is true. A coarse "we think we're in a
  blackout" flag (a later entry-reject sub-plan reads it to gate
  brand-new entries).
- **Per-trade record** `spread-blackout:rec:<trade_id>` —
  `{ trade_id, instrument, account, applied, opened_at, expires_at,
  pip_size, original_stops, cancelled_orders }`. The `applied` flag is the
  *fine* "we actually touched THIS trade" signal; `pip_size` is baked on at
  apply time so the cron can work in pips with no intent in hand;
  `original_stops` holds the **pre-widen** SLs to restore (populated by
  System 2); `cancelled_orders` holds each cancelled resting order's id +
  signed body to re-drive (populated by System 3). A trade may carry both at
  once (multi-shot: a widened open position **and** a cancelled re-entry
  order share one record); the watcher restores both before clearing.
- **Per-order signed body** `order:<broker_order_id>` — the raw signed alert
  body, TTL'd to the alert window. Written on successful placement, read by
  the apply cron to recover the intent behind a broker pending order, deleted
  once the recovery watcher has re-driven (or dropped) it.

The `spread-blackout:rec:` records (incl. `original_stops`, `pip_size`, and
`cancelled_orders`) and the `spread_blackout_window` marker surface in
`trade-control status`.

The 15-min cron's **recovery watcher** walks each `applied` record,
**restores every remembered stop to its original** (verbatim), then clears
the record — once the spread has recovered (sampled live via
`Broker::get_quote`, converted to pips via the record's `pip_size`) **or** a
~3h backstop has fired, whichever comes first, regardless of the clock. A
closed position (`AmendError::NotFound`) is benign — nothing to restore. A
*failed* restore is logged loudly (`console_error!`) and the record is still
cleared (a stranded record would re-detect forever; the backstop TTL is the
final net). Records the box never marked `applied` (e.g. because the edge
cron was missed while it was down) are left untouched. The recovered/elevated
cutoffs are coarse placeholders pending operator tuning — see the
`TODO(open-question)` in `src/spread_blackout.rs`.

## TradeNation session

TN routing requires the intent to name an `account:` (registered via
`trade-control account add`). The worker resolves the account through
the metadata index in KV, reads the per-account `TN_ACCOUNT_<NAME>`
credentials secret, logs in on demand, and caches the resulting
`Session` JSON in KV under `tn:session:<name>`.

The worker is self-healing: when a cached session is rejected (TN
expired it), the next request transparently re-logs in using the
stored credentials and writes the new session back to KV. No external
rotation is needed — credentials live in the secret, and sessions
regenerate themselves.

Register an account. The intended order is **broker first, worker
second**:

```sh
# 1. Provision the demo at TradeNation (or use an existing live
#    account). This populates the local encrypted store at
#    ~/.config/tradenation/accounts.enc with the credentials.
tradenation account create my-tn-demo

# 2. Register the same name with the worker. By default this reads
#    username + password from the local TN store — no re-typing, no
#    chance of a typo leaking bad credentials to Cloudflare.
trade-control account add my-tn-demo \
  --broker tradenation --kind demo \
  --admin-key-file ~/.config/trade-control/admin-key.hex
```

`account add --broker tradenation <name>` **errors** if `<name>` isn't
in the local TN store. Pass `--username <override>` if you intentionally
want to register a different identity than what's stored locally (it
will prompt for a fresh password).

This wraps `wrangler secret put TN_ACCOUNT_MY_TN_DEMO` and writes the
metadata entry in KV. After that, intents with `account: my-tn-demo`
route through that account; the worker handles session lifecycle.

### Market-hours entry blackout

A separate, simpler cousin of the spread-blackout above. It fixes a real
incident: an entry candle closed right at a US-index rolling-future's daily
close, the worker placed a **resting stop order** after the candle closed,
the order sat through the whole closed session, and triggered on the next
open's gap — getting stopped out on a move that never traded while the
market was open. The fix has two halves:

1. **A reject gate** — block a *new* entry that fires inside the
   instrument's daily close→open gap, so no fresh resting order is placed
   into a market that's about to close.
2. **A cron sweep** — act on a still-pending resting order that's *already*
   resting when the gap opens, per the operator's chosen `blackout_close`
   policy.

The per-instrument no-entry windows are **UTC minute-of-day ranges** derived
once a day by a 06:00 UTC cron (`src/cron/blackout_hours.rs`) from the
broker's session hours and stored in KV under `blackout-hours:<instrument>`.
One window is emitted **per close→open gap** (a market can have several in a
day, e.g. a maintenance gap plus the overnight gap), buffered `[close −3h …
open +1h]` and merged where they overlap. Brisbane→UTC is fixed `−600 min`
arithmetic (Brisbane is UTC+10, no DST); the DST correctness is inherited
from the broker feed's London→Brisbane conversion, so the worker links no
timezone tables. This is **distinct** from the spread-blackout's
reduced-liquidity "spread hour" — that's handled by the feature above; this
one only covers genuine close→open gaps.

#### Reject gate — block new entries inside the window

When an `enter` resolves, the worker reads the instrument's stored windows
and compares `now`'s UTC minute-of-day against them. If `now` falls inside
any window it rejects:

- **Outcome:** `rejected: market-blackout`, **HTTP 423 Locked** (same family
  as pause / cooldown / spread-blackout — the intent is valid, the condition
  is transient, a later fire can succeed).
- **Cheap, KV-only.** It's a single KV read plus a minute comparison — no
  broker round-trip — so it sits **ahead** of the (broker-touching)
  spread-blackout gate.
- **Reject, NOT delay.** Nothing is persisted and no re-fire is queued. The
  next signal bar re-triggers the alert and re-runs the check — once the
  market has reopened the same entry passes.
- **Does NOT consume the intent id.** Like every `Rejected`, this is a `Skip`
  in the replay-dedup path (no `mark_seen`), so the in-hours refire is
  allowed through (see "Replay protection scope" in `CLAUDE.md`).
- **Fail-open.** A KV read error, or an instrument with no derived windows
  (24-hour markets, unparseable session text, or windows not yet refreshed),
  yields an empty window set and the gate is a no-op — a transient hiccup
  must never block a legitimate trade.

Both the webhook and the server-side trade-plan engine dispatch entries
through `run_enter`, so this one gate covers both paths. The buffer defaults
(3h before close, 1h after open) live in `Buffers::default()`
(`core/src/intent/blackout/derive.rs`).

#### Cron sweep — pull a resting order caught in the gap

The reject gate only stops *new* entries. An order placed just before the
gap opened can still be resting when the close arrives — exactly the
incident. The `*/15` cron sweep (`src/cron/sweep.rs`) handles it: for each
tracked `EntryAttempt` it now checks the instrument's stored windows
**before** the SL-breach branch (across a closed session the last-traded
price is stale, so the closed market itself must be the trigger, not a
stale-price SL check). If the order is resting inside a window it acts on
the row's `blackout_close` policy, snapshotted from the intent at placement:

- **`CancelResting`** (default, the incident fix) — cancel the unfilled
  resting order only. It **never** closes a position: if the order already
  filled, the cancel is a broker no-op and the filled position is left
  untouched (its SL is the only thing that should close it — see the
  `veto_close_only_when_thesis_invalidated` rule in `CLAUDE.md`).
- **`CancelAndClose`** — also market-close any open position on the
  instrument. Opt-in only; the operator chose it at arm time because a
  partly-formed setup carried through a closed session isn't worth the
  reopen-gap risk.

The cancel logs with reason `market-blackout` (greppable apart from
`expired` / `bar-expiry` / `sl-breached`), then the row is deleted so the
next sweep doesn't re-process it. A KV read error fails open (empty windows
⇒ not due), matching the reject gate. Pre-field `EntryAttempt` rows decode
to the safe `CancelResting` default.

### Local TN store vs server-side account list

Two account namespaces exist:

- **`tradenation account list`** — local encrypted store
  (`~/.config/tradenation/accounts.enc`). Holds username + password
  for every TN session this machine can open.
- **`trade-control account list`** — the worker's metadata index.
  Maps `account:` strings on the wire to a broker + kind + caps +
  `TN_ACCOUNT_<NAME>` secret.

The names must match for TradeNation accounts. `account add` enforces
this. CLI-side TN catalog walks (used by `tv-arm --account-id=X` and
`trade-control instruments`) also log in via the named local entry, so
the log line names the account the operator passed instead of whatever
the default-demo pointer happens to be. If the local store doesn't
have a matching entry, the CLI errors with a hint to run
`tradenation account create <name>` first.

OANDA accounts are unaffected — they share one worker-wide
`OANDA_API_KEY` secret and don't need a local-store counterpart.

### `--account-id` shell completion

`tv-arm --account-id <TAB>` can complete from locally-known accounts
once the helper from `tv-arm --print-completions` is wired in. Source
your tv-arm completion file in zshrc, then add:

```zsh
compdef -e "_arguments -S '--account-id=[server-side account name]:account:_tv_arm_account_names'" tv-arm
```

The helper calls `trade-control account names`, which prints the union
of operator history and local TN store names — no admin key, no
network, safe to invoke on every TAB.

### Prerequisites

`trade-control account add` shells out to `wrangler` to push the
credential secret, so two things must be true on the machine running
the CLI:

- **Logged in to Cloudflare:** `wrangler login` (one-time per machine,
  or whenever the OAuth token expires). Without this `wrangler secret
  put` fails with an auth error after the metadata POST has already
  succeeded.

The CLI passes `--name` to wrangler itself (defaulting to
`trade-control-web-hook`), so `account add` / `account delete` work from
any directory — you no longer need to `cd` into the repo root. Override
the target Worker with `--worker-name <name>` or the
`TRADE_CONTROL_WORKER_NAME` env var if you've deployed under a different
name.

### Recovering from a half-done `account add`

If the metadata POST succeeded but the `wrangler secret put` shell-out
failed (wrong directory, not logged in, etc.), do **not** re-run
`trade-control account add` — the worker will reject it with `409
Conflict: already exists`. Push the secret directly instead:

```sh
read -s TN_PW
echo "{\"broker\":\"tradenation\",\"kind\":\"demo\",\"username\":\"<tn-username>\",\"password\":\"$TN_PW\"}" \
  | wrangler secret put TN_ACCOUNT_<NAME-UPPERCASED> --name trade-control-web-hook
unset TN_PW
```

(The `--name` flag is what `trade-control account add` passes for you —
it lets the command run outside the repo root.)

`<NAME-UPPERCASED>` is the account name uppercased with `-` → `_`
(e.g. account `my-tn-demo` → binding `TN_ACCOUNT_MY_TN_DEMO`).

Then verify with `trade-control account test <name>`.

If you hit a 503 with `tradenation login failed`, check the worker
logs — likely either a wrong-broker mismatch, a malformed credentials
blob, or TN itself rejecting the credentials.

### Adopting a manually-opened trade

The worker only tracks trades it placed itself — it does not poll the
broker for open positions. If you open a trade manually in the broker
UI (or by any non-worker path) and want the webhook lifecycle to run
against it (`close`, `pause`/`resume`, multi-shot re-entry gate after
the manual trade closes, SL-breach sweep), register it with
`POST /admin/adopt-trade` via the CLI:

```sh
trade-control adopt-trade \
  --account tn-reversals-demo \
  --trade-id hs-chf-jpy-efd5e647 \
  --instrument CHF/JPY \
  --direction short \
  --order-id 26773227 \
  --position-id 27169081 \
  --stop-loss 173.50 \
  --admin-key-file ~/.config/trade-control/admin-key.hex
```

The IDs come from the broker UI: on TradeNation the trade-detail panel
shows them as `Add Order` (= `--order-id`) and `Open Position` (=
`--position-id`). The worker calls `list_open_positions` on the named
account and rejects with **409** if instrument, direction, order id, or
position id don't all line up — typo'd ids do not silently land a row
that close alerts will then no-op against.

On success the worker writes a synthetic `EntryAttempt` keyed by
`(account, trade_id, 1)` — same shape every other alert path already
reads. `expires_at` is inferred from the seen-index by finding the
latest `expires_at` across any prior prep/veto/enter alerts that
landed for this `trade_id`, so close alerts sent within the original
H&S `not_after` window will find the row. With no prior alerts on
file the row falls back to a 4-day lifetime.

V1 supports TradeNation accounts only; OANDA adoption returns 501
(the verify path is mechanical but unwired).

## KV namespace

The worker uses Cloudflare KV for replay protection and instrument cooldowns.
Create the namespace once and paste its id into `wrangler.toml` under the
`TRADE_CONTROL_KV` binding:

```sh
wrangler kv:namespace create TRADE_CONTROL_KV
```

### Row lifetimes: no-TTL per-trade rows vs TTL'd control rows

KV rows fall into two classes with deliberately different lifetimes:

| Class | Rows | Lifetime | Cleaned up by |
|---|---|---|---|
| **Per-trade lifecycle** | `plan:` · `plan-state:` · `archived-plan:` · `entry-attempt:` · `order-body:` · `control-event:` | **No TTL** — live as long as the trade matters | `plan purge <trade_id>` |
| **Control / dedup** | `cooldown:` · `veto:` · `prep:` · `pause:` · `news:` · `spread-blackout:` · `blackout-hours:` · `seen:` · `retry-fire-seen:` | **Window-anchored TTL** — expiry *is* the intended behaviour | passive KV TTL |

The split exists because a per-trade row must outlive its own window. The plan
state row (`plan-state:`, the engine's per-trade watermark + FSM state) used to
carry a flat ~1-day TTL; when it aged out while the plan was still live, the next
cron tick read `None`, **re-seeded**, jumped the watermark to the newest candle,
and silently skipped any price-cross veto in the gap (bug #15). Now every
per-trade row is no-TTL and lives until you explicitly `plan purge` it — the
plan and its state can't fall out of sync by expiry. For the TTL'd control rows,
expiry is correct: a cooldown lapsing *is* the cooldown ending.

**Control-event audit trail.** A TTL'd control row vanishes passively when its
window lapses — KV writes no event and leaves no trace, so journaling a past
trade could see (from the R2 `req/` archive) that a control was *set* but nothing
recording that it *expired*. So every per-trade TTL'd control set (cooldown,
veto, prep, pause, news) also writes a small no-TTL
`control-event:{scope}:{trade_id}:{suffix}` row capturing
kind/name/instrument/set_at/ttl_seconds/computed_expiry/request_id. That trail
lets you reconstruct a control's set→expire lifecycle after the live row is gone.
It's append-only, read back via `list_control_events`, and dropped by `plan
purge`. (Global / instrument-only sets like `spread-blackout` / `blackout-hours`
aren't per-trade, so they carry no trail.)

R2 recording bundles (`req/` and `ticks/`) are likewise **no-TTL** now — they
persist until a `plan purge <trade_id>` (per-trade `ticks/`) or a bulk `purge
--older-than <days>` (date sweep over both prefixes) removes them.

### Index blobs are decode-tolerant

State for the `status` view is kept in five JSON-array index blobs
(`index:vetos`, `index:seen`, `index:preps`, `index:cooldowns`,
`index:prep-blocks`). These are read-modify-written on every veto / cooldown /
prep write. As the entry structs gain required fields, a single legacy element
written before a field existed could fail a strict whole-array decode and 500
*every* write that touches the index (this happened on 2026-06-12 — one
`trade_id`-less `index:vetos` element took all veto/cancel writes down
platform-wide).

The decode is now **element-wise tolerant**: a single element that fails to
deserialize is dropped with a `index decode: dropping bad element …` warning in
the worker log, and the next write rewrites the blob without it (self-healing).
A genuinely corrupt *container* (not a JSON array) is still a hard error.

The same tolerance applies to the per-key `pause:` / `news:` listings (read by
`status` and the news-window close gate): one value that won't decode is
dropped with a `kv list decode: dropping bad value key=… …` warning rather than
failing the whole listing. A KV I/O error on a read is still fatal.

If you ever need to clear a poisoned index by hand (it self-heals after one
write, so this is only an immediate unblock):

```sh
# namespace id is the TRADE_CONTROL_KV binding in wrangler.toml
wrangler kv key delete --namespace-id <id> "index:vetos"
```

Deleting an index key is safe — a missing key reads back as an empty list, and
the authoritative per-entry TTL keys (`veto:…`, `cooldown:…`) are untouched.

## Deploy

There are three environments, one per git branch, each an isolated worker
(own name, KV namespace, R2 bucket). The branch carries its own
`wrangler.toml`, so a plain `wrangler deploy` on a branch targets that
environment. See `DEPLOYED.md` for the full branch → environment model and
the staging → prod promotion rule.

**Every environment carries a suffix** (`-dev` / `-staging` / `-prod`). The
old no-suffix worker `trade-control-web-hook` + its R2 bucket
`trade-control-recording` are deprecated — kept running only until last
week's demo trades are journaled, then deleted. Do not deploy to them.

Use the per-environment deploy script — **never** call `wrangler deploy`
directly for a real deploy, because the scripts also rebuild and install
the matching CLIs:

```sh
git checkout main    && ./deploy-dev.sh       # dev     -> trade-control-web-hook-dev
git checkout staging && ./deploy-staging.sh   # staging -> trade-control-web-hook-staging
# ./deploy-live.sh is added at the first prod promotion.
```

Each script:

1. **Asserts the branch** matches the environment (won't let you deploy
   staging code to the dev worker).
2. `wrangler deploy`s the worker.
3. Rebuilds `trade-control`, `tv-arm`, `tv-news` with
   `TRADE_CONTROL_WEBHOOK` set so each binary **bakes that environment's
   worker URL** as its compiled-in default endpoint (`build.rs` →
   `BAKED_WEBHOOK`).
4. Installs the binaries into `~/.cargo/bin` under **suffixed names** —
   `trade-control-staging`, `tv-arm-staging`, `tv-news-staging` (and the
   `-dev` set). So you pick an environment by which command you run; no env
   var to set. The worker URL each `tv-arm-<env>` registers its `TradePlan`
   against is baked in the same way.

`deploy-lib.sh` holds the shared logic; the per-env wrappers hold only the
branch + URL (one place each), so standing up a new environment (e.g. the
upcoming `-prod`) is a one-script change.

> The legacy top-level `deploy.sh` is deprecated and now just points at the
> per-env scripts.

### Per-environment Pine versions

The Pine source (`pine-scripts/candle-signals-v2.pine`) carries **no
webhook URL**. One Pine source serves every environment; what differs per
environment is **which Pine *version*** a chart runs, so each `tv-arm-<env>`
reads the study version that matches its worker's server-side detector.

To pin a Pine version per environment, run **two studies on the chart with
distinct base titles** — e.g. `Candle Signals v24` and `Candle Signals
v25` — and point each environment's `tv-arm` at the one it should arm. The
deploy scripts bake the target study title the same way they bake the
webhook:

- `deploy-dev.sh` sets `ENV_PINE_NAME` → `build.rs` `BAKED_PINE_NAME`, so
  `tv-arm-dev` arms only the study whose **base title** (the part before the
  ` (args)` suffix, which `tv-arm` strips) equals that name.
- `deploy-staging.sh` bakes a different name, pinning staging to its own
  version regardless of what's published on the chart.

A plain `cargo install` with no env set falls back to the canonical
`Candle Signals` (kept in sync with
`trade_control_conventions::PINE_INDICATOR_NAME`).

> **The chart study must be renamed to match the baked name.** `tv-arm`
> matches by base title; if the baked name is `Candle Signals v25` but the
> study on the chart is still titled `Candle Signals`, the arm fails loudly
> with the list of titles it *did* find. Rename the study (TradingView →
> study settings → title) in lockstep with flipping `ENV_PINE_NAME`.
>
> Keep an **active alert on only the targeted study**. `Every Bar Close`
> fires per study, so an alert live on *both* versions would double-fire
> each M/W enter.

## Test locally

```sh
cp dev.vars.example .dev.vars   # add SIGNING_KEY plus the OANDA secrets
wrangler dev
```

Then sign an intent (set `not_after` in the future) and POST it:

```sh
./target/release/trade-control sign \
  --key-file ~/.config/trade-control/key.hex \
  --template intent.yaml --non-interactive \
  | sed 's/{{close}}/1.1000/; s/{{high}}/1.1020/; s/{{low}}/1.0980/; s/{{time}}/2026-05-13T12:00:00Z/' \
  | http POST localhost:8787 Content-Type:text/plain
```

## Chart-driven arming: `tv-arm`

The Rust `trade-control` CLI is the low-level signer — one intent at a time.
For real H&S setups you want **one chart annotation → the whole bundle armed**
(invalidation + pcl-exhausted vetoes, trade-expiry veto, two preps, an entry,
plus any opt-in close triggers). `tv-arm` (the Rust binary in `tv-arm/`) is
that frontend.

It reads the active TradingView chart via [tv-mcp](https://github.com/jacksonkasi1/tradingview-mcp)
(a Chrome DevTools bridge), extracts the H&S geometry from your drawings,
delegates `trade_id` minting and intent signing to `trade-control build-trade
--from-file`, and posts the resulting alert bundle straight to TradingView via
an inside-page fetch.

> **Deprecated:** `scripts/tv_arm_hs.py` was the original Python frontend.
> It was ported to `tv-arm` and is no longer updated for new behaviour
> (consolidated `06-close-on-reversal`, calendar auto-draw,
> instrument-lookup integration, etc.). Use `tv-arm` instead.

What you draw on the chart:

| Drawing | Label | Carries |
|---|---|---|
| Horizontal line | `too-high` or `too-low` | Invalidation veto trigger (right-shoulder price). Direction-sensitive: `too-high` for short H&S, `too-low` for long IH&S. |
| Trendline | (any in `BREAK_LABELS`) | Break-and-close prep level. Skip with `--skip-break-and-close`. |
| Trendline | (any in `RETEST_LABELS`) | Retest prep level. Skip with `--skip-retest`. |
| Fib retracement | (label optional) | Drives both TP (`2 × neckline − head`) and the `pcl-exhausted` veto price (`midpoint + 0.8 × (TP − midpoint)`). Draw spanning **head → neckline**. |
| Vertical line | `trade-expiry` | `not_after` for every alert in the bundle. |
| Vertical line | `<prep>-expiry` (`break-and-close-expiry`, `retest-expiry`; aliases `neckline-expiry` / `retrace-expiry`) | Cutoff for that prep: emits an `08-prep-expire-<step>` alert that blocks the prep once crossed, so a setup whose prep lands too late never enters. **tv-arm errors** if the line is in the future but its prep trend line is missing (the setup could never enter); **warns** if the line is in the past (re-arm later). |
| Vertical line pair | `news-start` / `news-end` | Each pair emits a `build-news` bundle. **Presence of any *live* pair also adds `news` to the consolidated `06-close-on-reversal` alert's `inside_window`** — no extra flag. **Only lines anchored inside the chart's visible window are armed** — off-screen lines are treated as stale leftovers and ignored. **A pair whose window has already fully elapsed (`end_time ≤ now`) is also dropped silently** — common when arming off a chart showing historical bars (the line is on-screen but its window has passed in wall-clock terms); there is nothing left to act on, so it is no longer a hard "stale blackout" rejection. |
| Vertical line pair | `blackout-start` / `blackout-end` (or `pause` / `resume` aliases) | Each pair emits a `build-pause` bundle. Blocks entries while active. **Only lines anchored inside the chart's visible window are armed** — off-screen lines are ignored as stale. **A pair whose window has already fully elapsed (`end_time ≤ now`) is dropped silently** rather than rejected, so arming off a historical chart no longer fails on a past blackout. |
| Horizontal line | `support` or `resistance` | Each line adds an `[lo, hi]` band of ±`--reversal-band-pct` (default `0.1%`) to the `06-close-on-reversal` alert's `sr_bands` list, and adds `price` to its `inside_window`. Multiple lines union. |

> **Single-slot roles are scoped to the visible window.** The invalidation,
> break-and-close, retest and TP-fib roles each fill exactly one slot. Before
> picking, `tv-arm` drops any candidate drawing whose time-span lies *entirely*
> outside the chart's on-screen window — in **both** live-arming and replay
> builds. This stops a stale off-screen drawing (e.g. a neckline left weeks
> away on the timeline) from being armed against just because it's the newest.
> Intersection (not containment) is used, so a line spanning the whole view or
> poking past one edge still counts; the `trade-expiry` marker additionally
> keeps a small forward margin past the right edge, where it's meant to sit.
> If more than one drawing for a single-slot role survives the window filter,
> the run-mode tiebreak still applies (live = newest) and a `warn` is logged so
> you can clear the clutter. A per-role `dropped_out_of_window` count is logged
> at `info`.

When news pairs *and* `support`/`resistance` lines are both present, a
single `06-close-on-reversal` alert is emitted with
`inside_window: [news, price]` — the close fires on an opposing reversal
candle when *either* gate matches (worker-side OR composition).

CLI:

```sh
cargo run -p tv-arm -- \
  --broker tradenation \              # or oanda; auto-detected from chart exchange
  --account-id ms-tn-1 \              # defaults to ms-<broker>-1
  --risk-pct 0.5 \                    # % of NAV (or --risk-amount <home-ccy>)
  --reversal-band-pct 0.1 \           # half-width % around support/resistance lines (default 0.1)
  --veto-on-reversal \                # experimental: a reversal off a band before entry also vetoes the upcoming trade (default off)
  --quasimodo \                       # alias: --skip-break-and-close --skip-retest --require-confirmation (drop both H&S preps, gate on a confirmed candle)
  --strategy-v2 \                     # arm BOTH a stop entry AND a Quasimodo limit entry on one setup; first to fire cancels the other (see below). Conflicts with --quasimodo/--entry-market/--skip-*; needs --max-retries > 0
  --no-breakeven \                    # disable break-even stop management (default ON at 50%; see "Break-even stop management")
  --breakeven-pct 0.7 \               # override the break-even arm threshold as a fraction of entry→TP (default 0.5)
  --skip-break-and-close \            # for stocks (no after-hours retests)
  --skip-retest \                     # implies --skip-break-and-close; for late entries
  --skip-golden \                     # drop the Pine golden-candle requirement (golden is required by default)
  --max-retries 5 \                   # multi-shot re-entry cap (default 5; pass 0 for single-shot). >0 keeps the engine plan in AwaitEntry across re-entries (see below)
  --require-confirmation \            # require a confirmed signal candle on entry (independent of golden). Also flips the recover-entry default to `limit` (see below)
  --recover-entry limit \             # H&S/iH&S: how to recover a stop entry gone wrong-side during confirmation — market | limit | abort. Omit to default off --require-confirmation (limit) else drop
  --blackout-close close \            # market-hours blackout: also flatten an open position if caught in the close→open gap (default: cancel = cancel the resting order only)
  --register-plan \                   # arm the trade: register one signed TradePlan with the server-side engine
  --shadow                            # register observe-only: engine evaluates + logs, but never places orders (safe dry watch)
```

Run `tv-arm --help` for the full flag surface — it has diverged from the
deprecated Python script.

### Dual entry — `--strategy-v2` (stop + Quasimodo)

`--strategy-v2` (H&S / iH&S only) arms **two entries on the same setup at
once**, competing for the same trade:

1. **Stop entry** — the normal one: gated by the break-and-close + retest
   preps and a confirmed signal candle, placed as a stop order that triggers
   on a break *through* the signal level.
2. **Quasimodo (QM) entry** — no preps at all, gated only on a confirmed
   signal candle. Its order spec is **identical to standalone `--quasimodo`**:
   a stop at the signal level (signal_low − 1 pip for a short) carrying a
   `recover_entry: limit` fallback. It fires as a resting stop on a normal
   break, and when the signal candle has already overrun the level the engine
   recovers it to a **limit** resting at the level — so it still fills on the
   pullback *back* to the level, the mirror of the break-through.

   > Earlier strategy-v2 builds armed this leg as a bare `EntrySpec::Limit` at
   > the level with *no* recovery. For a short whose confirmation candle closed
   > below the level, that limit is geometry-invalid (a sell-limit must rest
   > above market), so the engine rejected it and — with no recovery — dropped
   > the whole leg silently, forfeiting a winning entry (demo trade 031,
   > CAD/JPY). The QM leg now shares standalone `--quasimodo`'s exact spec so
   > the two can never diverge again.

This is **not** `--quasimodo`, which runs the QM setup *instead of* the stop
entry. strategy-v2 runs both. The bundle gains a second enter alert,
`09-enter-qm`, alongside `05-enter`; both share the trade's `trade_id`.

**Whichever fires first wins.** The two enters share that `trade_id` and a
non-zero `--max-retries`, so the worker's retry gate treats them as attempts
of one trade: when the second enter fires, the gate finds the first's resting
order and **cancels it** before placing the new one — but if the first has
already *filled* (an open position), the second is **rejected** instead (we
never stack two entries on one setup). The stop entry is emitted first, so on
the rare bar where both qualify simultaneously the stop wins the tie and the
QM fire is deduped.

Because the cancel-the-sibling mechanism rides on multi-shot, `--strategy-v2`
requires `--max-retries > 0` (it defaults to 5; `--max-retries 0` is
rejected). It conflicts with `--quasimodo`, `--entry-market`,
`--skip-break-and-close`, and `--skip-retest`.

### Server-side engine registration (`--register-plan`)

Trades are armed **server-side**: the worker evaluates every trigger itself on
its cron tick — there are no paid TradingView alerts. `--register-plan` folds
the **whole trade** — every condition (re-expressed as an engine `Trigger`),
plus the embedded enter/close/veto/prep intents — into one signed `TradePlan`
and POSTs it directly to the worker (action `register`). The plan rides the
same whole-body HMAC as every other intent (carried in the intent's
`trade_plan` field), so it can't be tampered.

> **The legacy TradingView-alert path has been retired.** `tv-arm` used to also
> POST a signed 5-alert bundle to TradingView via tv-mcp and let TV fire each
> alert at the webhook (`--create-alerts`). That whole path — the flag, the
> tv-mcp template, the `alert_spec` / `create_alerts` / `post_outcome` modules —
> is gone. The signed bundle is still written to disk as a build artifact, but
> arming is now solely `--register-plan`.

A failed register is a hard error, but the signed bundle is already on disk by
the time the POST happens, so the trade is never lost. The plan's destination
is the baked-at-build-time webhook, so `tv-arm-staging --register-plan`
registers against the staging worker with no extra flag. The chart timeframe
must map to an engine granularity (`1`/`5`/`15`/`60`/`240`/`D`), else the
register is rejected.

> **`--plan-out` is the offline sibling.** `tv-arm --plan-out <file>` *without*
> `--register-plan` builds the plan and writes its JSON to disk but never POSTs
> to the worker — used to replay / inspect a historical setup. Because such a
> setup is usually already in the past, the build relaxes its time-sensitive
> checks (`trade_expiry` already elapsed, an in-window news event) from hard
> errors to **warnings** in this offline mode, so the JSON still gets written.
> Any path that actually arms the worker (`--register-plan`, and the
> `build-trade --from-file` signing path) stays strict and rejects an expired
> `trade_expiry`. The strictness toggle is `BuildStrictness` in
> `cli/src/trade_patterns.rs`.

The worker validates the registered plan and **persists** it to KV (key
`plan:{scope}:{trade_id}`, **no TTL** — like an archived plan) for the
server-side engine to enumerate each cron tick. A registered plan never times
out; it is removed only when the engine retires it (archive + clear on a
terminal state: close, trade-expiry veto, or its window closing). The carrier
`register` intent's `not_after` is a short control TTL and is **not** the
plan's lifetime — anchoring KV expiry to it dropped live plans ~1h after arming
(the 2026-06-23 bug). The engine that *evaluates*
those plans — a state machine per trade — runs on the `*/15` tick and evaluates
both M/W (per-bar enter heartbeat) and H&S (the Rust port of the
`candle-signals-v2.pine` detector) entries plus the trendline / level / time
triggers and vetos.

> **Shadow mode for watching a new plan.** A live (non-shadow) plan dispatches
> its fired intents through the *same* `run_enter` / `run_close` handlers the
> webhook uses — it places **real broker orders**. To watch a new or changed
> plan's decisions without trading, register with **`--shadow`**: the engine
> evaluates the plan and advances its `PlanState` identically to a live plan,
> but logs each would-be fire as a `cron engine SHADOW would-fire:` line instead
> of touching the broker (no order, no seen-id mark). Scrape those lines from
> the Cloudflare Real-time Logs to confirm the plan does what you expect. The
> shadow/live choice is baked into the signed plan at arm time, so it can't be
> flipped in flight — re-arm to promote a proven setup to live. (Field:
> `TradePlan.shadow`, `#[serde(default)]` → live for plans registered before the
> flag existed.)

**Calendar / news bars are folded into the plan too.** A registered plan
carries not just the trade's own conditions but the **pause/resume** (blackout)
and **news-start/news-end** (news-window) control bars — both the operator's
chart-drawn pairs and the auto-fetched forex-factory events. Each becomes a `TimeReached` rule
carrying the matching `pause` / `resume` / `news-start` / `news-end` intent and
firing at the window edge; the engine fires them **non-terminally** (they set
the blackout / news-window KV state without ending the trade's spine) and the
cron dispatches them through the same handlers the webhook uses. In `--shadow`
they're logged, not applied, like every other fire. (Before this, a
`--register-plan` produced a plan with *no* calendar bars — the register POST
ran before the bundles were built; fixed in Stage E.10 / v37.) The folding lives
in `append_control_rules` (`tv-arm/src/trade_plan_build.rs`); the non-terminal
evaluation is `evaluate_controls` (`engine/src/evaluate.rs`).

The auto-fetched events are windowed over the **chart's visible range**
(`get_range().visible_range`), widened to the trade expiry when that sits past
the visible right edge. This is what lets you re-arm an **old** trade scrolled
back into view and still get the news bars it overlapped — events in the visible
window are kept even though they're all in the past relative to `now`. (Before
this, the calendar fetch was anchored to `now` and only looked forward, so an
old trade silently got *zero* news bars; manually drawn `news-start`/`news-end`
pairs were unaffected and always armed.) Both the auto-draw path
(`auto_draw_calendar_lines`) and the supplemental fetch when you've hand-drawn
some pairs (`discover_or_fetch_calendar_bundles` → `run_calendar_bars`) use the
visible window via the shared `calendar_window` helper.

The plan builder
is `tv-arm/src/trade_plan_build.rs` (the inverse of `alert_spec.rs`); the
`TradePlan` / `Trigger` model lives in `core/src/trade_plan.rs`; per-trade
engine state is `core/src/plan_state.rs`; the FSM evaluator is
`engine/src/evaluate.rs`; the candle-pattern detector port is
`core/src/signals.rs`.

#### Re-arming an existing setup (`--update`)

`tv-arm` mints a **fresh random `trade_id` every run**, so the engine treats
each re-arm as a brand-new plan — and the *old* plan keeps ticking in KV until
its TTL lapses. When you move annotations on the chart and re-run, pass
`--update` (only meaningful alongside `--register-plan`) so the prior plan is
deleted from the engine first:

```sh
# Auto-resolve by instrument: deletes the one existing plan on this instrument,
# then registers the fresh one. Hard-errors if more than one plan is registered
# for the instrument (re-run with the explicit id from `plan list`).
tv-arm-staging --register-plan --update ...

# Explicit: delete exactly this prior trade_id, then register fresh.
tv-arm-staging --register-plan --update hs-eurusd-a3f9c1d2 ...
```

`--update` reconciles the server-side engine plan: it POSTs `plan-list` to find
the target, then a signed `plan-delete` that clears the plan's `plan:` +
`plan-state:` KV (see `plan delete` below). A bare `--update` with no plan
registered for the instrument is a logged no-op. The resolution logic is the
pure `resolve_update_target` (`tv-arm/src/pipeline.rs`), unit-tested.

#### Inspecting / managing registered plans (`trade-control plan list` / `show` / `delete`)

Three subcommands let you see and manage what the engine is evaluating —
useful during the parallel-run period to confirm a plan registered, whether
it's in shadow mode, and how far its FSM has progressed:

```sh
trade-control-dev plan list                # compact table of every LIVE plan + state
trade-control-dev plan list --include-all  # also list terminated (vetoed/completed) plans
trade-control-dev plan list --yaml         # raw worker YAML (one entry per plan)
trade-control-dev plan show eurusd-hs-7    # full dump of one plan + its state
trade-control-dev plan show eurusd-hs-7 --yaml
trade-control-dev plan delete eurusd-hs-7  # drop a plan (live and/or archived)
trade-control-dev plan purge eurusd-hs-7   # wipe ALL KV + R2 traces of one trade
trade-control-dev purge --older-than 90    # bulk R2 sweep: drop req/ + ticks/ older than 90d
```

`plan list` shows `TRADE_ID`, `ACCOUNT`, `INSTRUMENT`, `SHADOW`, `PHASE`,
`RULES`, `FIRED` (the rule_ids that have latched), and `ARCHIVED` (the archive
timestamp, blank for live plans). The state columns (`PHASE`, `FIRED`, …) are
blank until a plan's first cron tick seeds its state row, so a freshly-registered
plan lists with empty state until the next `*/15` tick. `plan show <trade_id>`
scans every account scope for that id and dumps the whole `TradePlan` (every rule
+ embedded intent) plus the persisted `PlanState`. `plan list` / `plan show` are
read-only KV-only control actions (`plan-list` / `plan-show`), signed like
`status`, hitting the baked endpoint with no extra flag. A `plan show` for an
unknown id exits non-zero with `no registered plan with trade_id …`.

**Archived (terminated) plans.** When a plan reaches a terminal phase — a veto
fired, or the single-shot entry was dispatched — the engine deletes its live
`plan:` / `plan-state:` rows on that cron tick so it stops ticking. Before
deleting, it **archives** the plan (plan body + terminal `PlanState`) to an
`archived-plan:{scope}:{trade_id}` KV key, so a vetoed/completed setup can still
be analyzed afterward. Plain `plan list` shows only the live plans (what the
engine is still evaluating); `plan list --include-all` (alias
`--include-archived`) also lists the archived ones, marked by their `ARCHIVED`
timestamp. Use `plan show` / the R2 tick-bundles for the per-tick detail of why
a plan terminated. **Archived plans have no TTL** — they accumulate until you
delete them explicitly, so clean up with `plan delete` once you're done
analyzing.

`plan delete <trade_id>` is the inverse of `register`: it scans every account
scope and drops the matching `plan:` and `plan-state:` rows **and** any matching
`archived-plan:` row, so the engine stops evaluating a live plan and a terminated
plan is cleared from the archive. (A terminated plan usually exists only in the
archive, so this is what actually removes it.) It's KV-only and **idempotent** —
deleting a plan that doesn't exist returns `ok` (reported as a no-op), so
re-running is safe. The intended workflows: (1) `tv-arm` registers a plan and
draws its news/blackout lines; if you tweak or remove some, run `plan delete
<trade_id>` to wipe the stale server plan, then re-run `tv-arm` to register the
corrected one; (2) after a plan vetoed and you've analyzed it via `plan list
--include-all`, `plan delete <trade_id>` clears the archive.

`plan purge <trade_id>` goes further than `plan delete`: it removes **every**
per-trade trace once a trade is fully journaled. On top of the `plan:` /
`plan-state:` / `archived-plan:` rows that `plan delete` drops, it also clears
the trade's `entry-attempt:` rows (and each attempt's `order-body:`), its
`control-event:` audit trail, any enumerable trade-scoped `pause:` / `news:`
rows, and the trade's R2 `ticks/` bundles. Window-TTL'd `veto:` / `prep:` rows
are intentionally left to self-clear. Use it after you've finished analyzing a
trade and want it gone — the per-trade KV rows and R2 bundles are no longer
TTL'd (see "Row lifetimes" under the KV namespace), so an explicit purge is the
only thing that reclaims them.

`purge --older-than <days>` is the bulk counterpart for the recording bucket: a
retention sweep that deletes R2 objects under `req/` and `ticks/` whose date
prefix is older than the cutoff. It never touches KV — use `plan purge` for
per-trade KV cleanup.

Skipped preps are pre-fired directly to the worker so the entry's
`requires_preps:` gate is still satisfied — useful when joining a setup
after the break-and-close / retest already happened, or for stock setups
where those preps don't apply.

### Gotchas worth knowing

- **Trendline alerts need `extend_forward: true` in the payload.** TV's
  server-side cross evaluator only watches the segment between the two
  drawing anchors otherwise — so a prep that's supposed to fire when price
  crosses the neckline *after* the drawn anchor segment never fires. The
  drawing-level `extendRight` property does *not* propagate to the alert
  payload; we override unconditionally for trendline tools.
- **The engine interpolates trendlines in bar-index space, not
  wall-clock.** TradingView's x-axis is *ordinal*: closed sessions (nights,
  weekends, holidays) aren't plotted, so a neckline advances one step **per
  traded bar**, not per elapsed second. The server-side engine matches this
  — it counts the actual bars present in the broker candle feed between the
  two anchors (the feed elides closed sessions, confirmed on ALPHABET: an
  18 h overnight gap and a 66 h weekend gap each collapse to a single bar
  step). `tv-arm` bakes the chart's bar duration onto each `TrendlineCross`
  as a signed `bar_seconds`, used only as a fallback divisor when an anchor
  predates the engine's fetched candle window. A naive wall-clock
  interpolation would slide the break-and-close / retest level badly wrong
  on any gapped instrument (everything but 24/5 FX — and even FX gaps at the
  weekend). No market-hours table is needed: the candle feed *is* the
  ordinal axis. The engine **fetches its detector window back to the earliest
  trendline anchor** (`detector_window_for`), so every anchor lands in-window
  and the bar-index count is exact — the `bar_seconds` fallback is dead code
  for a normally-armed plan. It survives only as a belt-and-braces path for a
  pathological anchor older than the fetch could reach, and *that* path is
  **observable**: the engine attaches a warning to its `PlanEval` and the cron
  wrapper `rlog!`s it (`cron engine: plan <id> trendline …`) — a soft note when
  `bar_seconds` extrapolates across a gap, a hard one when a pre-`bar_seconds`
  plan (`bar_seconds = 0`) makes the trendline silently un-evaluable. If you
  ever see one, the anchor predates the fetch window — widen it, don't trust
  the extrapolation.
- **Chart-side `_alertId` binding is cosmetic.** The "link icon" on a
  drawing comes from a separate client-side binding that TV's GUI sets
  via `LineDataSource.setAlert()`. Programmatic creates can't easily
  populate it without facade-sync gymnastics. But the alerts still **fire**
  — the binding is only about whether the drawing shows the icon. Don't
  chase it.
- **TP via symmetric reflection.** `tv-arm` computes TP as `2 × neckline
  − head` from the fib's two endpoints, independent of which fib levels
  are visible / configured. Draw the fib spanning head → neckline.

### M/W (double-top / double-bottom) setups

M/W reversals use a completely different drawing input from H&S: **one
PATH (polyline) tool with 3 or 4 anchors**, plus a `trade-expiry`
vertical. No invalidation line, no neckline/retest trendlines, no fib.

The path anchors, **in draw order**:

1. **A — runup start** (audit/log only).
2. **B — first peak (M) / first trough (W)** — the SL anchor base.
3. **C — neckline retracement** — the entry/abort anchor.
4. **D — right shoulder** (*optional 4th anchor*) — the second peak/trough.
   Draw it to **arm the setup immediately** (see "4-point paths" below);
   omit it for the classic 3-anchor path where the worker discovers the
   right tower live.

Direction is inferred from the A→B leg geometry (A above B → W/long; A
below B → M/short) — the **path tool has no text label**, so detection
is geometry-only, and only a path whose anchors all sit inside the
visible chart range is picked up. `tv-arm` gates the setup at arm time:

- **Neckline-retracement depth.** Retrace as a % of the runup must be
  `< 40%`. `--allow-50-pct-m-trades` raises the ceiling to `<= 50%` for
  a marginal setup; `> 50%` is always rejected.
- **Right-shoulder alignment (4-point only).** When `D` is drawn, the
  taller of the two shoulders must stay **below the 1.3 extension** of the
  *shorter* shoulder (measured neckline→shorter), and `D` must sit on the
  same side of the neckline as `B`. A drawing that breaks either rule is
  **rejected at arm** with the offending levels printed.
- **Live broker spread.** The mid→bid/ask correction the worker applies
  needs the spread captured at arm time, so `tv-arm` **reads it live**
  from the broker (OANDA `/pricing`; TradeNation's chart bid/ask
  endpoint) and bakes it into the enter intent. There is **no override
  flag** — a failed read (no token, market closed, degenerate spread)
  **aborts the arm**. OANDA needs `OANDA_TOKEN` (or `OANDA_API_KEY`) in
  the environment.

Unlike H&S there is no prep chain and no re-entry (`max_retries: 0`):
the cancel/abort vetos or a fill end the setup. See the M/W bundle table
under "Alert basenames" above for what gets emitted.

**Worker-side real-time arming.** Unlike the book — a post-hoc method
that just stops at the neckline once *both* towers are printed — we arm in
real time with only the left shoulder (B) and neckline (C) known. So the
enter alert fires every bar close but the worker only arms the breakout
stop after **two** live confirmations, both on the neckline→peak (C→B)
leg, all MID-price:

1. **Right-tower window** — the bar's extreme (high for an M, low for a W)
   must reach **within 30% of the left-shoulder high** and stay below the
   1.3 extension:
   - **Floor `0.7`** — `neckline + 0.7 × (peak − neckline)`. A bar whose
     high (M) / low (W) never retraced this far back into the pattern is
     **declined** and the setup stays armed for the next bar. (Without
     this, a shallow poke past the neckline could arm a premature entry.)
   - **Ceiling `1.3`** — `neckline + 1.3 × (peak − neckline)`, the same
     extension the `mw-cancel` veto guards. A bar reaching it has
     invalidated the pattern; declined here too as a safety net in case
     the veto hasn't fired.
2. **"Middle of the M" cross** — a confirmed right tower says the shape is
   valid; the arming *trigger* is the bar that rolls back off it through
   the 50% level, `mid50 = neckline + 0.5 × (peak − neckline)`:
   - **M (short):** `high ≥ mid50 AND close < mid50` (crossed down).
   - **W (long):** `low ≤ mid50 AND close > mid50` (crossed up).
   - A bar that hasn't crossed is **declined** and the setup stays armed.

   Only after both confirm does the worker place the breakout stop at the
   neckline (book level, mid→bid/ask corrected). The fractions are fixed
   worker constants (`RIGHT_TOWER_MIN_FRAC` / `CANCEL_EXT_FRAC` /
   `MID_CROSS_FRAC` in `core/src/intent/mw_resolution.rs`).

   **A not-armed-yet bar is a benign decline, not an error.** Declining a
   bar here (right tower unconfirmed, middle not crossed, or breakout stop
   on the wrong side of the close) is the *expected* outcome on most M/W
   enter fires. The worker reports it as **HTTP 200** with
   `outcome: declined: mw-not-armed` — distinct from the **400
   `rejected: resolve-failed`** it returns for a genuinely malformed enter
   (wrong-side SL, entry outside SL..TP, sub-1R, missing field, bad
   script). Either way the setup stays armed (the decline is a seen-id
   no-op), but the wire status lets timeline/verdict tooling tell a routine
   decline apart from a real geometry bug. (Internally: the three arming
   gates return `ResolveError::NotArmedYet`.)

   **The cron-engine H&S `PinePattern` entry declines the same way.** A bar
   that fires the candle detector but whose enter can't pass the
   `needs_golden`/`needs_confirmed` gate or can't resolve to a valid bracket
   (e.g. a false-golden tiny pinbar with `signal_high ≈ signal_low` →
   degenerate geometry) is **declined this bar** — the plan stays in
   `AwaitEntry`, its veto rules keep being evaluated, and a later bar can
   re-form a valid pattern. It does **not** retire the plan. (Before this —
   bug #13 — a single-shot enter that fired the detector retired the spine to
   `Done` *regardless* of the dispatch outcome, silently abandoning the
   still-valid `close-positions` vetos. The pre-flight is the pure
   `pine_entry_dispatchable` in `engine/src/evaluate.rs`.)

   **A *multi-shot* enter (`max_retries > 0`) also keeps the plan alive — even
   after a *successful* fire.** A `FireMode::Once` enter normally sets
   `Phase::Done` the moment it fires, and the cron then archives + clears any
   `Done` plan (`src/cron/engine.rs`, `persist_plan_state`). For a single-shot
   enter that is correct. But a multi-shot enter *is* the
   place → fill → close (typically at SL) → re-enter-on-the-next-signal-bar
   mechanism, so retiring its plan on the first fire would archive the very plan
   that fires re-entry #2 — leaving the worker to enter once and never re-enter,
   even though the operator opted into multi-shot. So a multi-shot enter fires
   this bar but **stays in `AwaitEntry`**: the plan survives, its vetos keep
   ticking, and the next golden signal bar fires it again. The *placement cap* is
   not the engine's job — it is the worker's `retry_gate` (which the offline
   replay now also runs), so the engine just keeps emitting fires up to whatever
   `max_retries` resolves to. The plan still retires the normal way — a terminal
   `close-positions` veto, `trade-expiry`, or the enter's `not_after` window
   closing. (`engine/src/evaluate.rs` treats any `max_retries` other than the
   static default `Static(0)` as multi-shot, mirroring the worker's gate-entry
   check; commit `83333fa`. Verified on NZD/CHF 2026-06-19: the 07:30 golden
   short fired, stopped out, and should have re-entered on the 13:00 golden short
   pinbar — but the plan had archived at 07:30, so no re-entry fired.) The
   `retry_gate` itself now lives in `core` (`trade_control_core::retry_gate`,
   commit `edef1ea`), shared by the worker and the replay so both run the
   identical async cap/collapse logic against their own broker.

**4-point paths arm immediately.** Both live confirmations above exist only
because a 3-anchor path doesn't yet know the right tower — it discovers it
bar by bar. When the operator draws the **4th anchor (D — right shoulder)**
the second tower is *declared* (and validated at arm time), so both gates
are satisfied by construction and **skipped**: the setup is armed on the
first fire. Only the **1.3-extension ceiling** and the **stop-on-correct-side**
placement check still apply, and the cancel/mid levels are measured off the
**higher** of the two shoulders. The worker still re-measures every bar — a
higher shoulder reshapes the geometry (below) and the 1.3 ceiling still
aborts — matching "price may run higher but stay inside the 1.3 system".

**Worker-side dynamic geometry (KV-backed).** The book reads the *higher
shoulder* and the *deepest neckline* off a finished chart; we arm with only
the left shoulder + neckline known, so the worker recovers them bar by bar
and stores them per `trade_id` in KV (`mw-state:<scope>:<trade_id>`). On
each `Every Bar Close` fire of an M/W enter, before resolving:

- **Higher right shoulder** → recorded (body-based) and used as the SL
  anchor (the higher of left vs right for an M, lower for a W).
- **Deeper neckline** → a body that pulls below the current neckline but
  stays inside the **60% validity floor** of the runup→shoulder leg lowers
  (M) / raises (W) the neckline; entry/SL/TP re-derive off it.
- **Cancel** → a body past the 60% floor kills the setup: the worker
  cancels any pending order and writes a trade-scoped `mw-cancel` veto
  (which the `05-enter` lists, so later fires are blocked). It **never
  closes an open position** — `mw-cancel` is StopNextEntry-class.
- **Rogue wicks** → every comparison uses candle **bodies**
  (`max/min(open,close)`), so a lone wick can't move the shoulder/neckline
  or trip the cancel. Needs the `open` field (Pine v2.5+); a pre-`open`
  chart simply skips the dynamic update and rides the baked geometry.

Separately, an **`01-veto-mw-overshoot`** chart alert guards the *late-entry*
case: a `price crosses` alert at the **180% of top→neckline** level (= 80% of
the way from neckline to TP — the projected move is essentially complete). It
fires intra-bar (M on a low reaching it, W on a high), cancels the pending
order, and disarms — never closing an open position. Unlike the dynamic
neckline/shoulder above, this level is **static** (baked at arm time): Pine
can't move an alert and the WASM worker can't re-issue one, so if the pattern
later grows a higher shoulder / lower neckline the baked level only fires
*early* — over-vetoing (the safe direction: it blocks some valid late entries
but never lets a genuinely overshot trade through). It's the M/W analogue of
the H&S `pcl-exhausted` veto.

The dynamic-geometry decision is the pure `plan_mw_update` / `effective_mw_params`
(`core/src/intent/mw_state.rs`); the worker wraps it with the KV read/write
in `maybe_update_mw_state` (`src/lib.rs`). Baked params are the seed.

```sh
cargo run -p tv-arm -- \
  --broker oanda \
  --allow-50-pct-m-trades \           # opt in to a 40–50% neckline retrace
  --register-plan
# (draw the 3-anchor path + a trade-expiry vertical on the chart first)
```

### Dependencies

- Rust `trade-control` CLI on `$PATH` (or pass `--trade-control-bin`).
- A signing key at `~/.config/trade-control/key.hex`, matching the worker's
  `SIGNING_KEY` secret.
- tv-mcp checked out somewhere; `tv-arm` looks at
  `~/Downloads/tradingview-mcp-jackson` by default. Adjust the
  `--tv-mcp-root` flag if yours lives elsewhere.
- An active TradingView Desktop session in Firefox with DevTools open
  (tv-mcp connects via CDP).

## Chart annotation: `tv-news`

Sister binary to `tv-arm`. Annotates the active chart with one labelled
vertical line per upcoming forex-factory event affecting the chart's
instrument, so the downstream `tv_extract_*_trade.py` scripts (and `tv-arm`
itself when armed manually) have something to read from.

What it does:

1. Reads the chart's symbol + visible window via tv-mcp.
2. Resolves the symbol through `instrument-lookup` to get the asset's
   `news_currencies`.
3. Fetches forex-factory events spanning the visible window (multi-week —
   typical operator scroll is 2.5–3 weeks).
4. Filters to **2★ + 3★** for the asset's own currencies, plus **3★ USD**
   regardless of asset (so FOMC always lands on every chart).
5. Skips events that already have a tv-news vertical line within ±5
   minutes (idempotent re-run). Both the new `<ccy>-<n>-star-…` labels and
   the legacy `news-start` / `news-end` labels are recognised for dedupe.
6. Buckets the survivors by chart bar (per `state.resolution` — `"15"`,
   `"60"`, `"D"`, ...). Events sharing a bar get one drawing with a
   combined label.
7. Draws each bucket as a single vertical line. Single-event buckets are
   labelled `<currency>-<stars>-star-<name-slug>` (e.g. `usd-3-star-fomc`,
   `eur-2-star-cpi-y-y`); multi-event buckets concatenate every event's
   label, joined with `, ` and a newline every 3 events to keep the TV
   drawing-properties text box readable.

Note: this is purely chart annotation. The worker's news-window vetos and
the `tv-arm` arming flow continue to use the `news-start` / `news-end` /
`pause` / `resume` label vocabulary defined in `conventions`.

CLI:

```sh
cargo run -p tv-news --                    # default: draws lines + logs sentiment
  --dry-run                                # plan only, no drawing
  --dedupe-tolerance-min 5                 # ±tolerance for "already on chart"
  --tv-mcp-root ~/Downloads/tradingview-mcp-jackson
  --no-sentiment                           # skip the end-of-run sentiment summary
```

No `--broker` flag — news currencies are broker-agnostic. The chart can be on
any exchange (`TRADENATION:`, `OANDA:`, or bare symbol).

If the chart symbol isn't in the `instrument-lookup` catalog (e.g. a niche
commodity like `COCOA`), `tv-news` **warns and falls back to USD 3★-only
annotation** instead of aborting. The warning includes the overlay file path
so you can add an `[[asset]]` entry whenever you want the asset's own news
currencies to land on the chart.

### Sentiment summary

After the drawing phase, `tv-news` does a small follow-up fetch over the
recent past (24 hours by default, or back to Friday on Mondays so the weekend
isn't dropped), scores each **released** event for the chart's currencies,
and logs a verdict line:

- Per-event direction: `actual` vs `forecast` (falling back to `previous`),
  inverted for events where lower is better (unemployment, claims, deficit).
- Per-currency aggregate: weighted by impact (3★=3, 2★=2, 1★=1).
- Overall direction for the instrument: for FX pairs the quote-currency
  sentiment is inverted (bullish USD on EUR/USD = bearish pair); for
  indices/commodities the primary currency wins.
- Confidence: `high` (≥2 3★ events and ≥3 total, all aligned) / `medium`
  (≥1 3★ or ≥2 total) / `low`.

This is purely informational — it influences neither the drawings nor any
arming decision. The worker's news-window vetos still flow through
`tv-arm` and the `news-start`/`news-end` convention. Suppress with
`--no-sentiment`.

### Forex-factory disk cache

Both `tv-arm` and `tv-news` route every week-fetch through a shared on-disk
cache at `~/.cache/tv-arm/forex-factory/` (or `$XDG_CACHE_HOME/tv-arm/...`).
One JSON file per ISO week, named `YYYY-Www.json` (e.g. `2026-W22.json`).
TTL is **4 weeks from file mtime**, so historical-week files keep serving
replay/backtest runs for a month after first fetch.

To bust the cache for a specific week, delete its file; the next run
refetches and overwrites. Corrupt files (unparseable JSON) are silently
treated as a miss.

## Known limitations

- **Total open risk** is currently approximated by a count cap
  (`MAX_OPEN_POSITIONS`). A proper risk-percentage aggregator needs to read each
  open trade's stop-loss order; left as a follow-up.
- **Cross-currency pip values** are not handled — position sizing assumes the
  account currency equals the instrument's quote currency. Stick to majors
  where that holds (e.g. USD account + `*_USD` pairs) until generalised.
- **Pip size** is baked into the signed enter intent by `tv-arm` (from
  `instrument-lookup`), so JPY pairs and indices size correctly with no
  config. The `PIP_SIZE_<NAME>` secret is now only an override/fallback for
  intents armed outside `tv-arm`; the `0.0001` global default is the last
  resort. See "Pip size precedence" above.

## Self-hosting

The crate is structured around a `StateStore` trait so the same dispatch core
can run behind a non-CF transport. A native HTTP server adapter (axum + a
file-backed state store) is sketched in the plan but not yet implemented —
build that out if you want to run this on a home machine with dynamic DNS
rather than on Cloudflare Workers.
