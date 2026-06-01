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
- `close` — close all positions for the instrument. May also carry one of two
  worker-side gates: `require_news_window: true` (only fires inside an active
  news window for the `trade_id`) or `require_price_in_ranges: [[lo, hi], ...]`
  (only fires when the broker's current price sits inside at least one band).
  Both are armed by `tv_arm_hs.py` for the `06-close-on-reversal` and
  `07-close-on-sr-reversal` alerts respectively.
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
  with `require_news_window: true`) to flatten the trade on an opposing
  reversal candle.

## Alert basenames emitted by `build-trade`

When `tv_arm_hs.py` calls `trade-control build-trade --from-file`, the Rust
CLI mints a fixed-order bundle of signed YAMLs. Basename ordering matters —
the Python side maps drawings to alerts by prefix.

| Basename | Action | Fires on | Notes |
|---|---|---|---|
| `01-veto-too-high` | `veto` | Horizontal line crossing | Invalidation veto. Drawing-bound. Trade-direction sensitive (`too-low` for bullish IH&S). |
| `01-veto-too-low` | `veto` | Price crossing pcl-exhausted level | "Pattern completion level exhausted" — 80% of the way from the fib's midpoint to TP. Value-bound, computed by Python from the fib geometry. Direction-mirrors the invalidation veto. |
| `02-veto-trade-expiry` | `veto` | Vertical line crossing chart time | Hard stop: once the trade-expiry line passes, no more entries. |
| `03-prep-break-and-close` | `prep` | Trendline crossing (neckline break) | Skippable for stocks / late entries with `--skip-break-and-close`. |
| `04-prep-retest` | `prep` | Trendline crossing (retest from below) | Skippable with `--skip-retest`. |
| `05-enter` | `enter` | Pine `Candle Signals` golden candle | The actual trade. Gated on the preps above + opposing-direction veto absent. |
| `06-close-on-reversal` | `close` | Pine `Candle Signals` opposing reversal | Emitted only when news-pairs are drawn. Worker gates on an active `news:<trade_id>:*` KV entry — fires only inside news windows. |
| `07-close-on-sr-reversal` | `close` | Pine `Candle Signals` opposing reversal | Emitted only when `support`/`resistance` lines are drawn. Worker gates on the broker's current price being inside one of the `[lo, hi]` bands. |

Each news pair adds two more (`01-news-start-<id>` + `02-news-end-<id>`)
via a separate `build-news` shell-out, and each pause pair adds
`01-pause-<id>` + `02-resume-<id>` via `build-pause`.

## General workflow

The day-to-day loop, end to end:

1. **Draw the setup on a TradingView chart.** Mark the invalidation line,
   the neckline (break-and-close prep), the retest, a fib retracement
   spanning head → neckline, a `trade-expiry` vertical, and any optional
   extras (`news-start`/`news-end` pairs around scheduled news,
   `support`/`resistance` horizontals near key levels).
2. **Run `scripts/tv_arm_hs.py`.** It reads the chart geometry via tv-mcp,
   shells out to `trade-control build-trade --from-file` for `trade_id`
   minting + signing, then posts every signed alert into TradingView via
   an inside-page `fetch()`. Each alert lands as a configured TV alert
   pointed at your worker URL.
3. **TradingView fires alerts** as their conditions trigger (line
   crossings, Pine `Candle Signals` plots, time anchors). Each alert
   POSTs the cleartext signed YAML to the worker.
4. **The worker verifies the HMAC**, runs replay protection (the `id`
   field), applies any relevant gates (preps must be set, vetos must be
   clear, `require_news_window` / `require_price_in_ranges` for closes),
   then dispatches to OANDA or TradeNation. Outcomes are visible in
   Cloudflare Real-time Logs and via `trade-control status`.
5. **The scheduled `cron` trigger** (`*/15 * * * *`, declared in
   `wrangler.toml`) sweeps pending stop-entry orders for SL-breach
   independently of any TV alert. See `src/cron.rs`.
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
cooldown_hours: 12                     # only used by "invalidate"
```

`take_profit` can also be `{ from: high, offset_pips: 50 }` for a fixed
anchored TP. `offset_pips` is in instrument pip units; the default pip size is
0.0001 (good for major FX), override per instrument with the `PIP_SIZE_<NAME>`
secret (e.g. `PIP_SIZE_USD_JPY=0.01`).

**Stop vs limit entries:** a `stop` order fills when price moves *through*
the level (breakout: long stops sit *above* current price, short stops
*below*). A `limit` fills when price comes *back* to the level (pullback:
long limits sit *below* current price, short limits *above*). The worker
rejects the trade if the geometry is wrong (e.g. a long limit priced above
the current candle close), so a typo can't turn a limit into an instant
market fill at a worse price.

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

`id` is the **replay-protection key** — the worker remembers each id it's
fulfilled until just past `not_after`. Use a unique id per intended trade.

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

A `veto` is the inverse — a named blocker that must be absent for entry
to fire:

```yaml
v: 1
action: veto
instrument: EUR_USD
name: news-window
ttl_hours: 6
# level: cancel-pending   # optional; default stop-next-entry
```

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

## CLI

Build:

```sh
cargo build --features cli --release --bin trade-control
```

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
./target/release/trade-control veto EUR_USD news-window --ttl-hours 6 \
  --key-file ~/.config/trade-control/key.hex
# Escalated veto: also cancel resting pending orders for the instrument.
# Add --level close-positions to also close open positions.
./target/release/trade-control veto EUR_USD structure-broken --ttl-hours 4 \
  --level cancel-pending \
  --key-file ~/.config/trade-control/key.hex
./target/release/trade-control clear-prep EUR_USD break-and-close \
  --key-file ~/.config/trade-control/key.hex
./target/release/trade-control clear-veto EUR_USD news-window \
  --key-file ~/.config/trade-control/key.hex

# Stranded bad-name entries: `unlock` / `clear-prep` / `clear-veto` normally
# validate the instrument against the TradeNation catalog before sending,
# so a non-canonical name like `XAUUSD.F` is rejected with a candidate list.
# When the worker already holds such a string in KV, pass `--force` to
# skip validation and send the name verbatim — the only way to clear a
# stuck key short of `wrangler kv:key delete`.
./target/release/trade-control clear-veto "XAUUSD.F" too-low \
  --broker tradenation --force \
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
  - instrument: EUR_USD
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
| `PIP_SIZE_<INSTRUMENT>` | no | Override pip size, e.g. `PIP_SIZE_USD_JPY=0.01`. Default `0.0001`. |

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

Register an account:

```sh
trade-control account add my-tn-demo \
  --broker tradenation --kind demo \
  --admin-key-file ~/.config/trade-control/admin-key.hex
```

This wraps `wrangler secret put TN_ACCOUNT_MY_TN_DEMO` and writes the
metadata entry in KV. After that, intents with `account: my-tn-demo`
route through that account; the worker handles session lifecycle.

If you hit a 503 with `tradenation login failed`, check the worker
logs — likely either a wrong-broker mismatch, a malformed credentials
blob, or TN itself rejecting the credentials.

## KV namespace

The worker uses Cloudflare KV for replay protection and instrument cooldowns.
Create the namespace once and paste its id into `wrangler.toml` under the
`TRADE_CONTROL_KV` binding:

```sh
wrangler kv:namespace create TRADE_CONTROL_KV
```

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

## Chart-driven arming: `scripts/tv_arm_hs.py`

The Rust `trade-control` CLI is the low-level signer — one intent at a time.
For real H&S setups you want **one chart annotation → the whole bundle armed**
(invalidation + pcl-exhausted vetoes, trade-expiry veto, two preps, an entry,
plus any opt-in close triggers). `scripts/tv_arm_hs.py` is that frontend.

It reads the active TradingView chart via [tv-mcp](https://github.com/jacksonkasi1/tradingview-mcp)
(a Chrome DevTools bridge), extracts the H&S geometry from your drawings,
delegates `trade_id` minting and intent signing to `trade-control build-trade
--from-file`, and posts the resulting alert bundle straight to TradingView via
an inside-page fetch.

What you draw on the chart:

| Drawing | Label | Carries |
|---|---|---|
| Horizontal line | `too-high` or `too-low` | Invalidation veto trigger (right-shoulder price). Direction-sensitive: `too-high` for short H&S, `too-low` for long IH&S. |
| Trendline | (any in `BREAK_LABELS`) | Break-and-close prep level. Skip with `--skip-break-and-close`. |
| Trendline | (any in `RETEST_LABELS`) | Retest prep level. Skip with `--skip-retest`. |
| Fib retracement | (label optional) | Drives both TP (`2 × neckline − head`) and the `pcl-exhausted` veto price (`midpoint + 0.8 × (TP − midpoint)`). Draw spanning **head → neckline**. |
| Vertical line | `trade-expiry` | `not_after` for every alert in the bundle. |
| Vertical line pair | `news-start` / `news-end` | Each pair emits a `build-news` bundle. **Presence of any pair also auto-arms `06-close-on-reversal`** — no extra flag. |
| Vertical line pair | `blackout-start` / `blackout-end` (or `pause` / `resume` aliases) | Each pair emits a `build-pause` bundle. Blocks entries while active. |
| Horizontal line | `support` or `resistance` | Each line auto-arms `07-close-on-sr-reversal` as an `[lo, hi]` band of ±`--reversal-band-pct` (default `0.1%`). Multiple lines union. |

CLI:

```sh
python scripts/tv_arm_hs.py \
  --broker tradenation \              # or oanda; auto-detected from chart exchange
  --account-id ms-tn-1 \              # defaults to ms-<broker>-1
  --risk-pct 0.5 \                    # % of NAV (or --risk-amount <home-ccy>)
  --reversal-band-pct 0.1 \           # half-width % around support/resistance lines (default 0.1)
  --skip-break-and-close \            # for stocks (no after-hours retests)
  --skip-retest \                     # implies --skip-break-and-close; for late entries
  --require-golden \                  # require Pine golden-candle signal on entry
  --create-alerts                     # default; pair with --dry-run to inspect only
```

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
- **TP via symmetric reflection.** The script computes TP as `2 × neckline
  − head` from the fib's two endpoints, independent of which fib levels
  are visible / configured. Draw the fib spanning head → neckline.

### Dependencies

- Rust `trade-control` CLI on `$PATH` (or pass `--trade-control-bin`).
- A signing key at `~/.config/trade-control/key.hex`, matching the worker's
  `SIGNING_KEY` secret.
- tv-mcp checked out somewhere; the script's `TV_MCP_ROOT` constant points
  to `~/Downloads/tradingview-mcp-jackson` by default. Adjust if yours
  lives elsewhere.
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
- **Pip size** has a single global default of 0.0001 with per-instrument
  overrides via `PIP_SIZE_<NAME>`. Set overrides for JPY pairs and indices.

## Self-hosting

The crate is structured around a `StateStore` trait so the same dispatch core
can run behind a non-CF transport. A native HTTP server adapter (axum + a
file-backed state store) is sketched in the plan but not yet implemented —
build that out if you want to run this on a home machine with dynamic DNS
rather than on Cloudflare Workers.
