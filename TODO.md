# TODO — plain-mode enter must fire only on a signal PRINT, not on retroactive confirmation

## Bug (staging replay, AU200_AUD 2026-07-20 M15)

`tv-arm-staging --start=... --replay` (plain rules — no `--strategy-v2`, no
`--quasimodo`) entered off a signal that only *confirmed* on a later bar, not
one that actually *printed* there. Operator: "if we're not using strategy-v2
or quasimodo, we shouldn't accept confirmed signals — only signals when they
actually occur."

## Root cause

`core/src/signals/state_machine.rs::latched_signal_at` (the plain-mode path,
`needs_confirmed == false`) sets `fires = true` on TWO bar kinds:
1. a fresh signal PRINTS this bar  ← correct for plain mode
2. an earlier pending signal *just validated/confirmed* this bar (`just_valid`)
   ← confirmation semantics leaking into the plain path

Path #2 belongs only to `--strategy-v2`/`--quasimodo` (`needs_confirmed`),
which route through `first_confirmed_signal_at`, not `latched_signal_at`.

`latched_signal_at` is ALSO consumed by the close/guard path
(`eval_pine_guard` → `eval_pine_entry`), which legitimately reacts to a
reversal *confirming*. So the fix must NOT change `latched_signal_at` — scope
it to the plain ENTER only.

## Fix (DONE — code + tests green)

- [x] `eval_pine_entry` gained `print_only: bool`. When
      `print_only && !confirmed_first && sig.signal_bar_time != candle.time`
      → decline (a validated-here-but-printed-earlier signal is not an
      occurrence).
- [x] Enter call site passes `!rule.intent.needs_confirmed` (plain = print-only;
      confirmed enters opt INTO confirmation-firing).
- [x] Guard call site (`eval_pine_guard`) passes `false` — reversal-close still
      reacts to a reversal printing OR validating now (unchanged).
- [x] Tests: `plain_enter_does_not_fire_on_a_retroactive_confirmation_bar`
      (bar 3 of `two_short_engulfers_window` → no fire) +
      `plain_enter_fires_on_the_bar_the_signal_prints` (bar 2 → fires off the
      print). Both green.
- [x] engine 165 pass, core 879 pass; clippy clean; fmt clean.

## Operator clarification (both satisfied by the fix)

1. `--strategy-v2` after a break-and-close + a confirmed candle → ACCEPT.
   → the QM leg `09-enter-qm` (`needs_confirmed`) fires via
   `first_confirmed_signal_at`; `print_only == false` there. Pinned by
   `confirmed_enter_still_fires_on_the_confirmation_bar_after_print_only`.
2. No `--quasimodo` and no `--strategy-v2` → REJECT every confirmed signal.
   → plain enter `needs_confirmed == false` → `print_only`. Pinned by
   `plain_enter_does_not_fire_on_a_retroactive_confirmation_bar`.

## Done

- [x] CHANGELOG v105 (incl. the behaviour split) + memory
      `plain_enter_print_only_not_confirmation.md`.
- [x] staging: fix `feb6b65` (tag v105) + test `1185ad7`, pushed.
- [x] main: cherry-picked `4e3e551` + `9e0d768`, pushed.
- [x] parent submodule pointer advanced to `1185ad7`, pushed.
- [x] replay-candles now builds (market-hours WIP landed); suffixed CLIs
      (trade-control / tv-arm / tv-news / replay-candles, -staging + -dev)
      rebuilt with the fix + reinstalled; fix string verified in both replay
      binaries. Worker NOT touched (operator: "don't worry about the server").
- [ ] final: confirm the AU200_AUD 2026-07-20 replay no longer enters off the
      10:15 confirmation bar (unit tests already prove it; confirming end-to-end).

## Side-fix (v106) — replay warm-up back-off cap

Operator noticed the replay pulled 15k warm-up candles. Root cause: `--start` on
a Monday → naive 200-bar window lands in the weekend → AU200 returns 1 candle →
`next_pull_from` extrapolated off a size-1 sample and leapt ~17 months back.
- [x] Cap each back-off at `MAX_BACKOFF_SPAN_MUL` (4) × current span
      (`cli/src/bin/replay_candles.rs`); ATR floor (200 bars) intact.
- [x] Tests: caps-gap-poisoned-jump + cap-doesn't-shrink-healthy; suite 107 green.
- [x] CHANGELOG v106.
- [ ] commit/push staging + main + parent-bump + rebuild CLIs (staging done).
