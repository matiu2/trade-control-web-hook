# TODO — golden reversal candle off S/R closes the trade (v78)

## Operator rule (canonical)
A **golden opposite-direction reversal candle** closes the open trade **at close**
in exactly two contexts, OR-composed:
  1. off a **support/resistance line** (price close inside an SR band), OR
  2. **during a news window**.
Outside both contexts a golden opposite reversal is **ignored** (no close).

## Problem
EUR_USD short (plan hs-eur-usd-c285cc7c). The 2026-07-08 11:00 BNE (01:00 UTC)
candle was a **golden bullish pinbar off support (1.14032)** whose close
(1.14055) sat inside the reversal-close SR band [1.13918, 1.14146]. It should
have closed the short early. It didn't — trade rode back to break-even and
stopped out at 0R.

## Root cause (two parts)
1. **Engine AND vs worker OR.** `06-close-on-reversal` carried
   `inside_window: ["news","price"]`. The **worker** OR-composes the gates
   (price-in-band OR news-open) → would have closed. The **engine replay**
   `eval_pine_guard` AND-composes: it early-returns `None` when
   `wants_news && no-news-window-open` *before* checking the price band. At
   11am no news window was open → close silently skipped. Engine is stricter
   than live — wrong.
2. **No always-armed price-only reversal close.** The SR reversal was bundled
   into `06` alongside the news gate, so "off an S/R line" only worked when a
   news window was *also* open (per the engine AND-bug).

## Plan (user chose: Both; any golden opposite-dir pattern)
- [x] **Engine OR-fix**: `eval_pine_guard` OR-composes news+price via new
      `close_windows_pass`, mirroring `evaluate_close_gates`. 2 regression tests
      (both-windows fires on price-in-band w/ no news; declines when neither).
      All 130 engine tests pass.
- [x] **Split 07-close-on-sr-reversal**: `build_sr_reversal_close_alert`
      (price-only, always-armed, carries veto_on_reversal) +
      `build_news_reversal_close_alert` (news-only). Caller emits each on its own
      gate. 6 CLI tests rewritten for the split; all pass.
- [x] `veto_on_reversal` moved to the 07 (SR) half.
- [x] Trigger mapping for `07` basename already exists (trade_plan_build.rs:288).
- [x] Tests: engine OR + CLI split. 130 engine, 259 cli, 810 core, 22 worker,
      214 tv-arm all green.
- [x] Replay the EUR case: 07-close-on-sr-reversal FIRES at 11:00 BNE
      (close 1.14055 in band) → **+1.03R** early close (was 0R break-even SL).
- [ ] README + CHANGELOG (v78) + memory.
- [ ] commit/push/tag v78, merge to staging, deploy dev+staging, bump parent gitlink.

## Note: root `src/lib.rs` (dead Cloudflare stub) fails `cargo build --workspace`
Pre-existing on main (`broker_oanda::login` cfg'd out) — NOT my change. Deploy
builds specific `-p` packages, not the root lib. Verified base main breaks too.

## A change isn't complete until: tests pass, clippy+fmt clean.
