# TODO — journal timing diff stamps the firing bar (2026-09-21)

- [x] `live_fires` stamps each fire with `fired[].candle.time`, not the cron's `tick_ts`
- [x] fall back to the tick when a fire carries no candle (older bundle schema)
- [x] fixture + tests: late cron is clean; a real one-bar divergence still reported
- [x] timeline view deliberately unchanged — it is a wall-clock event log

# TODO — per-instrument D1/H4 session anchor (webhook side)

- [x] bump `tradenation-api` to `broker-tradenation-v0.20.0`; `[patch]` instrument-lookup to the local path
- [x] TN adapter: both fetch paths use `get_candles_range_aggregated_on` + `session_anchor(instrument)`
- [x] CLAUDE.md hazards; BUG doc statuses
- [x] repair the stale-binary re-bless (separate commit)
- [x] cache migration: `rebuild-h4` on the whole TN table + anchored OANDA names; TN Australia 200 D rows deleted
- [x] `local-chart`: no code change needed (bars come through candle-cache); restart it
- [ ] replay: `replay.rs:567` NY-close edge sampling assumes H4 lands on the NY close — only matters for the spread-blackout marker on anchored instruments (their masks are not NY-based anyway); not changed
- [x] session spans baked (`market-hours-gen --session-out`), `market_session`, `StoredReason::MarketClosed` park + promote at the open
- [x] replay: newest park wins in `armed_verified`; 32 goldens re-blessed with a note

# TODO — `--spec-url` accepts a pasted local-chart browser URL

## Why

The operator reads a chart at

    http://127.0.0.1:8790/?instrument=GBP_JPY&tf=h4&broker=tradenation&goto=2026-08-24T08%3A17%3A35Z

and wants to arm off exactly those drawings. Today `--spec-url` demands the
`/arm-setup?instrument=…&tf=…&broker=…` form, so the operator has to hand-edit
the path and delete `goto` — a two-step transcription whose failure mode is
silent: drop `broker=` and the arm reads OANDA's drawings instead
(`422 missing required roles`, or worse, a wrong-chart arm that succeeds).

Both forms carry the same chart identity (broker + instrument + tf). tv-arm
should accept either and normalise internally.

## Scope

- [x] `tv-arm/src/spec_url.rs` — `normalise(&str) -> Result<String>`:
      rewrite the path to `/arm-setup`, keep `instrument` / `tf` / `broker`,
      drop everything else (`goto` is a view hint, not chart identity).
- [x] `/arm-setup` URLs pass through **unchanged** — journal builds them
      (`journal/src/tv/local_chart.rs::arm_setup_url`) and that must not move.
- [x] Call it once, at the single seam: `pipeline.rs::read_setup_from_url`.
- [x] `--spec-url` doc comment mentions both accepted forms.

## Out of scope

- journal's `arm_setup_url` — it already builds the canonical form.
- Any change to what `/arm-setup` requires or answers.

## Status

- [x] 10 unit tests + 1 seam test; the seam test mutation-checked (removing the
      `normalise` call makes it fail with the un-normalised URL — the shape of
      bug where a correct helper has a caller that never calls it)
- [x] implementation
- [x] 529 tv-arm tests pass; `cargo clippy --all-targets` clean; `cargo fmt`
- [x] verified end-to-end against the live :8790 (a GET writes nothing): the
      operator's own pasted URL armed `TRADENATION:GBPJPY` h4, H&S short,
      8 alerts, `source=…/arm-setup?instrument=GBP_JPY&tf=h4&broker=tradenation`
- [ ] committed + pushed, parent pointer bumped
