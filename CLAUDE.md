# trade-control-web-hook — orientation for Claude

Two layers live in this repo:

1. **Rust worker + CLI** (`src/`, `core/`, `cli/`, `broker-oanda/`) — the
   Cloudflare Worker that receives signed TradingView alerts and the
   `trade-control` CLI that signs them. The README covers the wire format,
   actions (`enter`/`prep`/`veto`/...), and CLI subcommands in depth.
2. **Chart-driven Python tool** (`scripts/tv_arm_hs.py`) — reads a
   TradingView head-and-shoulders chart via tv-mcp and produces the full
   alert bundle for one setup by shelling out to `trade-control build-trade
   --from-file`. The README has a section on it; this file is the
   "stuff a future Claude will get bitten by" deeper note.

Read the README first for the user-facing story. This file is for hazards.

## Things the README doesn't shout

### tv_arm_hs.py: server-side trendline-cross eval is anchor-bounded

Burned a lot of time on this. When you POST a `create_alert` with
`tool: "LineToolTrendLine"`, TV's server only evaluates price crossings
inside `[base_time + offset1*resolution, base_time + offset2*resolution]`
unless you set `extend_forward: true` in the payload. `stateForAlert()`
on the drawing returns `extendForward` based on the *drawing's own*
extension property — which is almost always `false` for an H&S neckline.
**Always force `extend_forward: true` for `LineToolTrendLine` alert
payloads.** Horizontal- and vertical-line alerts are unaffected.

### tv_arm_hs.py: the chart-side `_hasAlert` binding is a red herring

The "link icon" you see on a drawing when you create an alert via TV's
GUI comes from `LineDataSource.setAlert(alertId)` being called locally,
which writes `_alertId` onto the shape and registers a client-side
subscription. Programmatic creates can't easily replicate this — the
alerts facade is module-private and racing it via polling never wins.

This doesn't matter for alert *firing*. Server-side eval works fine
without the binding. If a future investigation says "the link icon is
missing", the answer is "yes, that's expected; don't fix it." Only
chase the binding if alerts genuinely aren't firing — and if so, look
at the *geometry* first (see above), not the binding.

### tv_arm_hs.py: TP geometry

Take-profit price is computed as `2 × neckline − head` from the fib's
two endpoints. Symmetric reflection through the neckline. This is
independent of which fib levels are visible. The user draws the fib
spanning head → neckline; the script does the reflection. If the user
draws the fib differently (e.g. shoulder → neckline, or with both
endpoints inside the range), the formula breaks. Heads up.

### Submodule registration

This directory is treated as a submodule of `trading-libraries` (parent
repo holds a `160000` gitlink to a commit here), but it is **not**
registered in the parent's `.gitmodules`. So:

- `git submodule status` in the parent **doesn't show this repo**.
- Updating: commit + push *inside* this repo first, then in the parent
  `git add trade-control-web-hook && git commit && git push` to advance
  the pointer.
- The parent's `CLAUDE.md` (long architectural doc) lists this repo
  under "regular directories" — that note is outdated; treat it as a
  submodule.

### Build-trade pipeline contract

`tv_arm_hs.py` writes a `trade.yaml` to a temp dir and calls
`trade-control build-trade --from-file <trade.yaml> --key-file <key>
--output-dir <dir>`. The Rust side mints `trade_id`, writes
`manifest.yaml`, and emits 5 signed alert YAMLs with fixed basename
ordering:

```
01-veto-<too-high|too-low>
02-veto-trade-expiry
03-prep-break-and-close
04-prep-retest
05-enter
```

The Python script maps these basenames to drawing roles by prefix.
If you rename a basename in the Rust pipeline, update
`build_alert_spec()` in the script to match.

### `--risk-amount` is not plumbed through

The script's `--risk-amount` flag errors out because `TradeSpec` (Rust
side, `cli/src/trade_patterns.rs`) doesn't have a `risk_amount: Option<f64>`
field. To make it work end-to-end:

1. Add `risk_amount: Option<f64>` to `TradeSpec`.
2. Plumb it through `assemble_trade` → `build_enter_alert` so it lands
   on the `Intent::Enter` as `risk_amount`.
3. The worker already handles `risk_amount` on enter intents.

Not done because the user hasn't asked yet — flagged here so the next
session doesn't re-investigate.

### `--dry-run` doesn't propagate to the worker

Same gap: `Intent::Enter` has a `dry_run: bool` field but `TradeSpec`
doesn't expose it. The script's `--dry-run` only controls whether
alerts get posted to TradingView; the entry alert, if it ever fires,
will execute live on the broker. To make `--dry-run` actually dry-run
the broker side: add `dry_run: bool` to `TradeSpec`, plumb through.

## Conventions

- The Rust side follows the parent's CLAUDE.md (2024 edition, no
  `mod.rs`, etc.).
- The Python side is single-file (`scripts/tv_arm_hs.py`, ~800 lines).
  Don't split into a package until a second strategy (M/W) lands.
- tv-mcp is *not* vendored — the script `subprocess`-launches Node
  scripts that import from `~/Downloads/tradingview-mcp-jackson`.
  Hard-coded path, fine for now (one-user tool).
