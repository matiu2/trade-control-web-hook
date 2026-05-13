# TODO

## Done

- **YAML wire format + interactive template-driven encoder** — landed. Interactive prompts wired through `dialoguer` (gated behind the `cli` feature).
- **Queryable state endpoint + CLI state-management client** — landed.
  `status` and `unlock` actions go through the same encrypted envelope as
  enter/close/invalidate. KV maintains `index:cooldowns` and `index:seen`
  JSON arrays alongside the TTL keys so `status` can list them. CLI gains
  `status` and `unlock <INSTRUMENT>` subcommands that POST to the deployed
  worker via reqwest::blocking. 68 lib tests pass.

## Phase 2 ideas (parked — captured for later, not building yet)

### Multi-stage trendline workflow

Instead of one single alert fires-and-enters, a setup is built up by a *chain* of TradingView alerts, each advancing a state machine inside the webhook:

1. **Break-and-close alert** — TradingView fires when price breaks and closes through a hand-drawn trendline. Worker records `setup:<id>` in pending state.
2. **Retest alert** — fires when price retraces back to the trendline. Worker advances the setup to "armed".
3. **Entry-candle alert** — fires on the next candle's signal. Worker only places the order if the setup id is `armed` and not invalidated.
4. **Pre-fill SL-hit alert** — a separate alert at the planned stop-loss price. If this fires before the entry-candle alert, the setup id is locked out for 12h (and any pending order cancelled).

Implications for the encrypted intent format:
- Add a `setup_id` field separate from the per-message `id` — the chain shares one `setup_id`, each alert has a unique `id` for replay protection.
- Add an `expected_state` field per alert: `expect: break_close | retest | entry | invalidate_at_sl`. Worker transitions the state machine instead of blindly placing an order.
- `StateStore` needs a `get_setup_state(setup_id)` / `set_setup_state(setup_id, state, ttl)` pair on top of the current `seen` / `cooldown` pair.

This is significantly more state than what the MVP carries; design it after the simple pin-bar flow is proven in live use.

When this lands, the CLI gains a third subcommand `list-setups` — show all setup state machines and their current state. Reuses the existing `status` / `unlock` plumbing.

### Carried-over blocker

`cargo build --target wasm32-unknown-unknown --lib` currently fails inside
`oanda-client` with a `BidAskDataSource: Send` regression — pre-existing,
not introduced by anything in this repo. Needs an upstream fix in
oanda-client before `wrangler deploy` will work again.
