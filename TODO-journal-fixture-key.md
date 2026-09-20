# TODO — journal `f` key: re-bless / record a fixture, and `raw replay`

Adds two bindings to the journal TUI's Replay page (and the list), driven by the
info-bar's existing fixture status.

## Decisions taken (operator, 2026-09-21)

* **The missing `--skip-bcr` is NOT a bug.** Checked the live plan
  `hs-aud-nzd-ff8e66e8` on staging: it carries `03-prep-break-and-close` and
  `04-prep-retest`, and its `05-enter` has `requires_preps: [break-and-close,
  retest]`. `BcrPreps::tv_arm_skip_flags` derives the flags from exactly those
  rules, so emitting nothing is the faithful reproduction. Arming that setup
  `--skip-bcr` would have been an *arming* choice; forcing it on the replay
  would make the replay diverge from the plan. **No change.**
* **Re-bless scope is this plan's matched cells only**, never the whole corpus.

## Tasks — ALL DONE

- [x] `f` on a plan **with** a fixture → re-bless its matched cells.
      `replay-candles-<env> --test-mode --fixture <CELL> --fixtures-dir <DIR> --rebless`,
      once per cell in the matched base group. Per-cell ok/fail in the report;
      a partial run is reported as an **error**, not an info line.
- [x] `f` on a plan with **no** fixture → prompt for a message, then capture.
      The message rides through the chart-load park
      (`save_fixture_pending: Option<Option<String>>`) so a deferred capture
      cannot silently drop it.
- [x] A one-line modal text prompt (`journal/src/prompt.rs`), separate from the
      `/` search prompt — that one is list-scoped, live-filtering, and its
      `Esc` means "drop the filter" rather than "cancel the action".
- [x] `R` → **raw replay** of the stored plan. Passes `--instrument` AND
      `--start` always (see hazards).
- [x] Tests first; **6 mutations run, all caught** (f always captures; parked
      message dropped; partial re-bless as info; `--instrument` dropped;
      `--fixtures-dir` dropped; `--start` dropped).
- [x] `cargo clippy` clean, `cargo fmt` run, 189 tests green.
- [x] Verified live in tmux against the staging worker: footer hints, prompt
      swallowing `q`/`x`/`R`/`r`/`f` as text, Esc cancelling, "fixture 2 ✓" →
      `f` → "re-blessed 2/2 cells" with `meta.message` preserved and
      `expected.json` rewritten, and `R` producing the same report as the
      hand-run CLI.

## Found while building (not in the original scope)

**The raw replay needed `--start` too, not just `--instrument`.** Running the
first version by hand against the real CLI died:

    bad-input: the replay window runs backwards: it ends 9.8 days before it
    starts (start 2026-09-18 20:00:00 UTC, end 2026-09-09 02:00:00 UTC)
    start = ... from the TradingView chart's start; override with --start

`resolve_window` ranks the chart above the plan for the window start exactly as
it does for the instrument, so for any plan whose chart has moved on the raw
replay is impossible without `--start`. It now passes the plan's `armed_at` —
the same cursor the re-armed replay uses, so the two runs cover the same window
and stay comparable.

## Hazards to respect

* **ALWAYS pass `--fixtures-dir`** — the runtime default walks up from cwd and
  has silently re-blessed another checkout's corpus before (19 of 63, no error).
  `fixtures::default_dir()` already honours `TRADE_CONTROL_FIXTURES_DIR`; the
  rebless must pass the *same* resolved dir it matched the cells in.
* **ALWAYS pass `--instrument`** to a raw replay. `resolve_window` ranks the
  TradingView chart symbol ABOVE the plan, so omitting it lets whatever pair
  the chart is on pick the candle feed — a wrong feed reports a plausible 0R.
* `--rebless` rewrites **only** `expected.json`; the hand-written `meta.message`
  survives. That is why re-bless is right here and a re-capture is not.
* `--rebless` refuses under `--simulate false`, `--cron-gap != 1`, `--upkeep`.
  We pass none of those.
