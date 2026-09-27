# TODO — the plan's broker must reach the drawing mechanism

`a` (DrawGeometry) drew a TradeNation plan's arm roles onto the OANDA chart.
`draw_geometry_current` sourced `instrument` + `granularity` from `PlanRow`,
which has **no broker field** — so no broker reached `draw_plan_roles` and
local-chart applied its default (OANDA). The operator watched an empty
TradeNation chart while every write answered 201.

`start_load_tv` already does this correctly (broker from the fetched
`PlanDetail`, parking behind a timeline fetch when the detail isn't loaded yet)
and `spec_url_for` sources it the same way. This makes `a` match them.

## Steps

- [ ] test: the geometry job carries the plan's broker
- [ ] test: pressing `a` before the detail is loaded parks and fetches, rather
      than drawing on a guessed broker
- [ ] `draw_geometry_current` sources `detail.broker`, parking on a miss
- [ ] thread broker through `spawn_draw_geometry` → `cli::draw_plan_geometry`
      → `draw_plan_roles`
- [ ] refuse an EMPTY broker with a clear message (`PlanDetail::broker` is `""`
      when no rule intent carried one) rather than drawing on the default
- [ ] clippy + fmt

## Done

- [x] test: the geometry job carries the plan's broker
- [x] test: pressing `a` before the detail is loaded parks and fetches
      (mutation-checked: defaulting the broker instead makes it fail)
- [x] test: an empty broker is not a broker to draw with
- [x] `draw_geometry_current` sources `detail.broker`, parking on a miss
- [x] threaded through `spawn_draw_geometry` → `cli::draw_plan_geometry`
      → `draw_plan_roles`
- [x] `replay-candles --positions` emits `broker` from `--source`, so `R` and
      `tv-arm --new-tv` land on the right chart too
- [x] local-chart-client 0.4.0
- [x] clippy + fmt, 227 journal tests green
