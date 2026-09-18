# TODO — replay: entry-instant quote + frozen upkeep series in fixtures (feat/replay-upkeep-fixtures)

From staging 2241c56b. Worktree `../trade-control-web-hook-stop-floor-mask`.
Claimed on ./llm-work.txt (main checkout); the other session has not started any of it.

## Why

Three gaps stop a D1 fixture from exercising the spread-gate park (steps 1-3 of
the 2026-09-18 plan, now on staging):
(a) TradeNation's aggregated D1 close book is the last H1 close — the calm
    print BEFORE the rollover — so the replay's synthesised entry quote never
    trips the gate. Live samples the first print AFTER the close.
(b) fixtures carry no finer series; `--upkeep` was live-window only.
(c) even under `--upkeep`, the sample was cleared before the enter dispatch.

## Steps

- [x] 1. `ReplayBroker::QuoteSample {at,bid,ask}` replaces the raw candle sample;
      `UpkeepTicks::opening_at(now)` = OPEN book of the finer bar opening at `now`.
      The per-bar pass (and the order-control tail) sets it after the tick walk and
      clears it after `order_control_pass`. Fixes (a)+(c).
- [x] 2. `UpkeepTicks::{to_frozen,from_frozen}` + `upkeep_bars.json` in
      `fixture::{save,load}`; `run_frozen` walks it; `--save` under `--upkeep` is
      allowed (refusal lifted); `--rebless` under a live `--upkeep` still refused.
      Fixes (b).
- [x] 3. Tests: upkeep unit (opening_at, frozen round trip); fixture round trip +
      corrupt-file-is-an-error; replay entry-point differential (calm close +
      wide 21:00Z open book ⇒ park ⇒ fill on the 01:00Z bar; control fills 21:00Z;
      calm open book does not park). Mutation-check the entry-point test.
- [x] 4. README (fixture files list + `--upkeep`/`--save`), CLAUDE.md note. clippy,
      fmt, commit, push.
- [ ] 5. Re-arm AUD/NZD D1 from ~/Downloads/AUD_NZD-TRADENATION-d-20260917T175936.json
      via `tv-arm --spec-in … --plan-out`, replay with `--upkeep 15m --save` into the
      main checkout's replay-fixtures/, confirm the park+promote shows in the
      timeline. Do NOT re-bless any other fixture.
