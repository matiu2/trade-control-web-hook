# TODO — positions emit, and the TradingView deprecation

The goal: `replay-candles` stops knowing about charts. It simulates and emits
**what happened**; a chart layer decides whether to draw it and where.

Today `--annotate` reaches out to a TradingView bridge from inside the
simulator. That is why the drawing code is shaped around TradingView's position
tool (tick offsets, a sidecar file of entity-ids) and why adding a second chart
would otherwise mean teaching the simulator about a second chart.

## Stage 1 — `replay-candles --positions <path>` ✅

- [x] `positions_out.rs` — its own wire types, mirroring `FireResult` rather
      than deriving `Serialize` onto a simulator internal
- [x] `--positions <PATH>` on `ReplayArgs`, emitting beside (not inside) the
      `--annotate` block
- [x] instrument + granularity written **unresolved** — resolving to a broker's
      convention is the consumer's job
- [x] `taken` carried explicitly, so a consumer need not re-derive the
      taken/not-taken split from `outcome`
- [x] 7 unit tests + 1 end-to-end test over a real fixture replay
- [x] `--positions` **conflicts with** `--test-mode` (see below)

### The bug this stage found

`--test-mode --fixture <name> --positions <path>` replayed three trades,
printed a full report, exited 0, and wrote **no file**. The emit sits on the
live replay path; `--test-mode` goes through `replay_one_fixture`, which never
reaches it. All 7 unit tests were green throughout.

Fixed by refusing the combination at parse time rather than adding a second
emit site — a flag that appears to work and silently produces nothing is worse
than one that says no. Pinned by
`replay_args::tests::positions_is_refused_with_test_mode_rather_than_silently_ignored`.

## Stage 2 — `tv-arm --new-tv [URL]` draws them ✅

- [x] `local-chart-client` as a dependency of `tv-arm`
- [x] `--new-tv [URL]` on `tv-arm`, bare form defaulting to
      `local_chart_client::DEFAULT_LOCAL_CHART_URL`
- [x] `tv-arm replay --new-tv` injects `--positions <tmp>`, reads the file back
      and draws via `DrawingsClient`
- [x] the colour rules moved to `tv-arm/src/replay_positions.rs` —
      presentation belongs to the chart layer, not the simulator
- [x] **stable drawing ids** (`replay-pos-<direction>-<fill_epoch>`), so a
      redraw upserts in place and this path needs NO sidecar manifest
- [x] prior drawings cleared by id prefix, never by wiping the chart
- [x] 9 unit tests + 5 argv tests + 2 live tests against a real server

### Deliberate choices

- **A plain `Option<String>`, not a backend enum.** `journal` uses a
  `ChartBackend` enum, which is right for permanent dispatch — but here TV is
  being deprecated, so the flag goes opt-in → default → gone. An enum would be
  added now and unpicked later.
- **Fail-soft drawing.** The plan is armed and the replay has already printed
  by the time drawing runs. A chart that cannot be drawn on is a missing
  picture, not a wrong answer — so it warns and returns `Ok` rather than
  turning a successful arm into a non-zero exit.
- **`--new-tv` does not disturb `--annotate`.** Both run, so one replay paints
  both charts while the new path is compared against the old. Pinned by
  `new_tv_does_not_disturb_the_tradingview_annotate_default`.
- **An unknown direction is refused, not guessed.** Defaulting to long would
  draw a coherent bracket for the opposite trade.

### Mutation-verified

Removing the `ID_PREFIX` filter from `clear_prior` — so cleanup wipes every
drawing on the chart — turns
`positions_land_a_rerun_replaces_them_and_the_operator_is_untouched` red with
"the operator's own drawing survived both runs". That is the guarantee that
makes running a replay on a working chart safe.

## Stage 3 — the deprecation (weeks away)

- [ ] `--new-tv` becomes the **default**
- [ ] delete `cli/src/bin/replay_candles/annotate.rs`
- [ ] drop `--annotate` / `--annotate-unfilled` / `--tv-mcp-root`
- [ ] drop the `trading-view` dependency from `trade-control-cli`
- [ ] `~/.config/trade-control/replay-annotations.json` becomes dead — remove

A deletion, not an unpicking. Nothing new is being built on top of the
TradingView path: `--positions` sits **beside** `--annotate`, never inside it,
and the two compose so both can run on a single replay while they are compared.

### ⚠️ The sidecar manifest is TradingView-scoped

`annotate.rs`'s `~/.config/trade-control/replay-annotations.json` holds
**TradingView entity-ids**. Nothing local-chart-shaped may share it, or a run
will try to `draw remove` local-chart ids against TradingView. The local-chart
path should not need it at all — see the stable-ids item in stage 2.

## Deferred

- `tv-arm` keeps its name for now. If local-chart is eventually renamed to
  "tv", the name stops being wrong on its own.
