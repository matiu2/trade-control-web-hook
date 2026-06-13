# Trade Control Web Hook

Cloudflare Worker that receives TradingView alerts and controls OANDA /
TradeNation trades. The body is cleartext YAML with an HMAC-SHA256
signature, so a leaked webhook URL can't be weaponised by anyone who
doesn't also have the signing key.

Thirteen actions are supported. The first five are the day-to-day trading
verbs; the rest are state management for multi-event setups and scheduled
windows.

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
| `02-veto-trade-expiry` | `veto` | Vertical line crossing chart time | Same hard stop as H&S. |
| `05-enter` | `enter` | Pine **`Every Bar Close`** alertcondition (every closed bar, not the golden/short-pattern plots) | Carries the baked static M/W params (`mw:` block); the **worker** computes entry/SL/TP from those + the live shell OHLC each bar, mid→bid/ask corrected with the arm-time spread. `max_retries: 0`, no preps. |

There is **no `06-close-on-reversal`** for M/W — the take-profit is a
hard 1R, so there's no opposing-reversal close to arm.

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
     bar-expiry independently of any TV alert, **and** runs the
     spread-recovery watcher (see below).
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

SL/TP rules reference the plaintext shell prices by anchor
(`close`/`high`/`low`) with a pip offset, so the CLI never needs to
know the live price — TradingView fills it in at fire time.

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
entry: { type: market }                # or { type: stop, from: high, offset_pips: 2 }
                                       # or { type: limit, from: low,  offset_pips: 5 }
                                       # stop entries may add on_too_close: see below
stop_loss:   { from: low,  offset_pips: -2 }    # anchored — or { absolute: 1.86236 }
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
cooldown_hours: 12                     # only used by "invalidate"
```

`take_profit` can also be `{ from: high, offset_pips: 50 }` for a fixed
anchored TP. `offset_pips` is in instrument pip units, scaled by the
instrument's pip size to a price.

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

**Stop vs limit entries:** a `stop` order fills when price moves *through*
the level (breakout: long stops sit *above* current price, short stops
*below*). A `limit` fills when price comes *back* to the level (pullback:
long limits sit *below* current price, short limits *above*). The worker
rejects the trade if the geometry is wrong (e.g. a long limit priced above
the current candle close), so a typo can't turn a limit into an instant
market fill at a worse price.

**Stop-entry "too close to market" fallback (`on_too_close`):** when a
stop-entry's trigger has already been overtaken by price by the time the
order tries to rest, the broker rejects it as "entry too close to / on
the wrong side of the market" (TradeNation `#19-10`). By default that
placement fails (HTTP 502) without poisoning the intent id, so the next
signal bar can retry. A `stop` entry can opt into a recovery instead:

```yaml
entry:
  type: stop
  from: high
  offset_pips: 1.0
  on_too_close:               # optional; default = skip (today's behaviour)
    action: market            # market | limit | skip
    max_slippage_pips: 8.0    # required for action: market — guard rail
```

- `action: skip` (default, also when `on_too_close` is omitted) — fail
  the placement, don't poison the id, let the next bar try.
- `action: market` — re-place as a **market order**, but only if the
  current price is within `max_slippage_pips` of the original stop
  trigger; otherwise fall back to skip and log why. The guard rail is
  **required** (rejected at validate time if missing) so a runaway
  breakout can't be chased into a much worse fill. The re-place is a
  **single** synchronous attempt and is re-sized against the actual
  market fill reference (a worse fill changes the stop distance and
  therefore the 1%-equity position size). It does **not** consume a
  multi-shot `max_retries` slot — it's the same intended entry.
- `action: limit` — **not yet implemented**; currently treated the same
  as `skip` (the entry stays retryable). Reserved for a follow-up that
  re-places the level as a pullback limit with geometry validation.

The distinct rejection is observable in the logs as
`entry-failed: too-close-to-market` (vs the generic
`entry-failed: broker rejected the order`). Only TradeNation has a
confirmed `#19-10` today; the OANDA path maps its broker rejections to
the generic case and does not trigger this fallback.

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

`id` is the **replay-protection key** — the worker remembers each id it
**successfully fulfilled** until just past `not_after`. Gate rejections
(missing prep, active veto, `allow_entry` script returning false,
cooldown, paused, etc.) and broker failures do **not** consume the id —
the same alert can refire and try again. Successful entries, completed
closes, and accepted state-set actions (prep, veto, pause, news-*,
clear-*, unlock) all consume the id, so byte-identical replays of those
return 409 instead of executing twice. Use a unique id per intended
trade.

## Conditional entries (preps + vetos)

Some setups want to fire `enter` only after a sequence of prior events.
The classic example is "break-and-close below the trend line, retest from
below, then entry candle." Each event is its own TradingView alert; the
worker stores short-lived named flags per-instrument and the `enter`
intent declares which flags must be set (and which must not).

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

For the quickest setup, eval the completions for your current shell
(detected from `$SHELL`) straight from your shell rc:

```sh
# ~/.zshrc (or ~/.bashrc)
eval "$(trade-control --print-completions)"
```

To write a static completion file for an explicit shell instead, use the
`completions <shell>` subcommand:

```sh
trade-control completions zsh > ~/.zfunc/_trade-control
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
key as auth. Set `TRADE_CONTROL_ENDPOINT` once to skip retyping `--endpoint`:

```sh
export TRADE_CONTROL_ENDPOINT=https://trade-control.<account>.workers.dev

# Dump active cooldowns, preps, vetos + recent seen ids as YAML.
./target/release/trade-control status \
  --key-file ~/.config/trade-control/key.hex

# Clear a cooldown set by an `invalidate` you didn't mean to send.
./target/release/trade-control unlock EUR_USD \
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

All control subcommands use the same replay-protection mechanism as the
trade actions — re-running the same `unlock` (or `clear-prep`, etc.)
within its window won't double-fire.

## Secrets

| Name | Required | Notes |
|---|---|---|
| `SIGNING_KEY` | yes | 64-hex-char HMAC-SHA256 key. Used to sign / verify the body and (re-used) to gate `GET /diag/*`. |
| `OANDA_API_KEY` | for OANDA | OANDA v20 token. |
| `OANDA_ACCOUNT_ID` | for OANDA | OANDA account id. |
| `OANDA_LIVE` | no | `true` for live trading; defaults to practice. |
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
price and restores them after. Cancelling resting orders is still a later
sub-plan.

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

The elevated cutoff is a **provisional single constant**
(`SPREAD_BLACKOUT_ELEVATED_PIPS`, 8 pips). It and the recovery cutoff
(`SPREAD_BLACKOUT_RECOVERED_PIPS`, 4 pips) now live **together** in
`src/spread_blackout.rs` so the hysteresis pair (`recovered < elevated`,
so the window doesn't flap) is tuned in one place. The whole feature works
in **pips** consistently — the cron side converts the broker's absolute
`ask − bid` to pips via the `pip_size` baked onto each per-trade record at
apply time. Both cutoffs are provisional — calibrate on demo before
relying on them.

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

This release lands the **state machine + cron skeleton** plus System 1 and
System 2. It does **not** cancel resting orders yet (a later sub-plan).

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
  System 2). `cancelled_orders` stays **reserved** for the order-cancel
  sub-plan and is empty for now.

Both surface in `trade-control status` (the `spread_blackouts` list — where
the `original_stops` and `pip_size` are operator-visible — and the
`spread_blackout_window` marker).

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

```sh
wrangler deploy
```

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
| Vertical line pair | `news-start` / `news-end` | Each pair emits a `build-news` bundle. **Presence of any pair also adds `news` to the consolidated `06-close-on-reversal` alert's `inside_window`** — no extra flag. |
| Vertical line pair | `blackout-start` / `blackout-end` (or `pause` / `resume` aliases) | Each pair emits a `build-pause` bundle. Blocks entries while active. |
| Horizontal line | `support` or `resistance` | Each line adds an `[lo, hi]` band of ±`--reversal-band-pct` (default `0.1%`) to the `06-close-on-reversal` alert's `sr_bands` list, and adds `price` to its `inside_window`. Multiple lines union. |

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
  --skip-break-and-close \            # for stocks (no after-hours retests)
  --skip-retest \                     # implies --skip-break-and-close; for late entries
  --require-golden \                  # require Pine golden-candle signal on entry
  --require-confirmation \            # require a confirmed signal candle on entry (independent of golden)
  --create-alerts                     # POST to TradingView; omit to only write the signed bundle to disk
```

Run `tv-arm --help` for the full flag surface — it has diverged from the
deprecated Python script.

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
PATH (polyline) tool with exactly 3 anchors**, plus a `trade-expiry`
vertical. No invalidation line, no neckline/retest trendlines, no fib.

The 3 path anchors, **in draw order**:

1. **A — runup start** (audit/log only).
2. **B — first peak (M) / first trough (W)** — the SL anchor base.
3. **C — neckline retracement** — the entry/abort anchor.

Direction is inferred from the A→B leg geometry (A above B → W/long; A
below B → M/short) — the **path tool has no text label**, so detection
is geometry-only, and only a path whose 3 anchors all sit inside the
visible chart range is picked up. `tv-arm` gates the setup at arm time:

- **Neckline-retracement depth.** Retrace as a % of the runup must be
  `< 40%`. `--allow-50-pct-m-trades` raises the ceiling to `<= 50%` for
  a marginal setup; `> 50%` is always rejected.
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

**Worker-side second-peak confirmation.** The enter alert fires every
bar close, but the worker only arms the breakout stop when the bar shows
a real second peak/trough — its extreme (high for an M, low for a W)
must fall inside a window on the neckline→peak (C→B) leg:

- **Floor `0.7`** — `neckline + 0.7 × (peak − neckline)`. A bar that
  closes just past the neckline but whose high (M) / low (W) never
  retraced this far back into the pattern is **declined** and the setup
  stays armed for the next bar. (Without this, a shallow poke past the
  neckline could arm and fill a premature entry.)
- **Ceiling `1.3`** — `neckline + 1.3 × (peak − neckline)`, the same 1.3
  extension the `mw-cancel` veto guards. A bar reaching it has
  invalidated the pattern; declined here too as a safety net in case the
  veto hasn't fired. Both fractions are fixed worker constants
  (`SECOND_PEAK_MIN_FRAC` / `CANCEL_EXT_FRAC` in
  `core/src/intent/mw_resolution.rs`); all comparisons are MID-price.

```sh
cargo run -p tv-arm -- \
  --broker oanda \
  --allow-50-pct-m-trades \           # opt in to a 40–50% neckline retrace
  --create-alerts
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
