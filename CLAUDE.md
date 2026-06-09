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

### "retry" / `max_retries` does NOT mean retrying failed placements

This naming has bitten more than one debugging session. `max_retries`,
the "retry gate" in `src/retry_gate.rs`, the `EntryAttempt` rows, and
the `is_retry_fire_seen` / `mark_retry_fire_seen` KV keys are all
**multi-shot re-entry** mechanisms: place → fill → close (typically
at SL) → a fresh signal bar arrives in the same alert window → place
again, up to `max_retries` total placements.

What "retry" is **not**:

- Not a retry of broker placement errors. A failed `place_order`
  returns 502 and does **not** mark the id seen — the next fire of
  the same alert body is allowed through (see "Replay protection
  scope" below). This was different before the 2026-06 fix; older
  worker versions wrote `mark_seen` on every dispatcher outcome
  including failures, which silently broke within-window legitimate
  refires.
- Not a retry of pre-broker rejections (veto, cooldown,
  `allow_entry` script failures). Those don't burn an `EntryAttempt`
  slot at all, and as of the same 2026-06 fix they don't poison the
  intent id either.
- Not a way to refire the same alert payload after a *successful*
  entry. Top-level intent-id dedup still applies on `Ok` outcomes —
  multi-shot needs distinct intent ids per fire (which `build-trade`
  mints for multi-shot setups, not for single-shot).

### Replay protection scope

The intent-id seen index (`is_seen` / `mark_seen`) covers two
distinct cases with different semantics — keep them straight:

1. **Entry-bearing actions** (`Enter`, `Close`, `Invalidate`,
   escalated `Veto`) — `mark_seen` is written **only on
   `ActionResult::Ok`**. `Failed` (broker error → 502) and every
   flavour of `Rejected` (gate failure, validation, state error)
   are logged via `console_log!` / `tracing::info!` for post-mortem
   visibility but deliberately do **not** consume the intent id.
   The next fire of the same alert body is allowed through.
   Implemented by `record_dispatcher_outcome` →
   `seen_decision` in `src/lib.rs`.

2. **Control actions** (`prep`, level-1 `veto`, `pause`, `resume`,
   `clear-prep`, `clear-veto`, `status`, `news-start`, `news-end`,
   `unlock`) — `mark_seen` is written on **every** completion via
   the `record_seen` helper. These are state-set ops where
   idempotency is legitimate (a replayed `prep` message shouldn't
   double-refresh the TTL).

**Why this matters.** A real incident on 2026-06-02 (CHF/JPY): an
`enter` alert fired 6 times in a 9h window. Fire 4 reached the worker
post-parse-fix, was correctly rejected with `rejected: missing-prep
(break-and-close)` because the prep alert had not fired yet — and
the old "every outcome marks seen" rule poisoned the intent id for
the remaining ~47h of the alert window. Fires 5
(`signal_confirmed=1`, the entry the operator wanted) and 6 both
409'd on `is_seen` at `src/lib.rs:154` before reaching the
`allow_entry` script gate. Operator entered the trade manually.

Pattern an unwary refactorer might fall back into: "let's mark seen
on every dispatcher outcome too, for visibility in `status`." Don't.
The cost of poisoning legitimate retryable rejections far outweighs
the visibility gain — and gate rejections are already visible in
Cloudflare Real-time Logs via the `console_log!` line in
`log_skip`.

If you find yourself looking at a 409 on an `enter` and wondering
"but the prior fires all failed — why didn't the retry gate let it
through", remember: the *seen-id replay check* and the *retry gate*
are two different things. With the 2026-06 fix in place, failures
no longer poison the id at all — so a 409 on an `enter` now means
**a prior fire of this id succeeded** (entered, closed, etc).
Before that fix, a 409 could just mean "we logged some prior
non-Ok outcome." If you're still seeing surprising 409s post-fix,
look at the *latest* recorded outcome via `trade-control status` to
see what the prior successful fulfilment actually was.

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

### Risk and dry-run plumbing

`TradeSpec` carries both `risk_amount: Option<f64>` and `dry_run: bool`
since 2026-05. The enter-alert builder honours `risk_amount` over
`risk_pct` when set (the worker validator rejects both), and propagates
`dry_run` only onto the enter intent — vetos and preps never carry it.

On the Python side: `--risk-amount` adds `risk_amount: <n>` to the spec;
`--broker-dry-run` adds `dry_run: true`. The Python `--dry-run` flag is
unrelated — that one short-circuits before any POST to TradingView.

### Pip size is baked into the enter intent, not looked up by the worker

Since 2026-06 the worker does **not** consult `instrument-lookup` (it's
WASM, no catalog linked) and no longer relies on the
`PIP_SIZE_<instrument>` secret as the *source of truth*. `tv-arm` reads
`asset.pip_size` from `instrument-lookup` at arm time and bakes it onto the
signed enter intent:

- **Top-level `Intent.pip_size: Option<f64>`** — set for **both** H&S and
  M/W enters. The worker reads this first (`run_enter`, `src/lib.rs`);
  `pip_size_for` (secret → `0.0001` default) is only the fallback for
  intents with no baked value.
- **`MwParams.pip_size`** — M/W also carries pip inside `mw` (its
  mid-correct resolution reads that copy directly). `tv-arm`/the cli set
  both fields to the same number, so don't "fix" one without the other.

`pip_size` is a signed field (whole-body HMAC), so a baked value can't be
tampered. It's also bound into gate scripts as the `pip_size` variable
(`core/src/rules.rs`). The catalog itself: indices report `pip_size = 1.0`
(the sizing point), *not* their sub-point tick — see the
`[[pip_size_vs_tick_size]]` memory and the `instrument-lookup` fix. Don't
reintroduce a worker-side pip lookup or a tick-derived index pip.

## Conventions

- The Rust side follows the parent's CLAUDE.md (2024 edition, no
  `mod.rs`, etc.).
- The Python side is single-file (`scripts/tv_arm_hs.py`, ~800 lines).
  Don't split into a package until a second strategy (M/W) lands.
- tv-mcp is *not* vendored — the script `subprocess`-launches Node
  scripts that import from `~/Downloads/tradingview-mcp-jackson`.
  Hard-coded path, fine for now (one-user tool).
