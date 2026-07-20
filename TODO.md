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

## Remaining

- [ ] CHANGELOG + memory note.
- [ ] Commit/push staging + cherry-pick main + tag + parent-bump.
- [ ] Rebuild replay-candles / tv-arm CLIs IF the market-hours WIP that
      currently breaks the replay-candles binary has landed (else note blocked —
      the engine fix is in the shared crate regardless).
