# TODO

## Active — TradeNation session auto-rotation

Operational shim until the worker can re-authenticate itself.

- [x] Cron-driven `TN_SESSION_JSON` refresh — `scripts/refresh-tn-session.sh`
      logs in via the local `tradenation` CLI and pushes the result with
      `wrangler secret put`. Installed at `0 */2 * * *`.
- [x] Manual probe (deleted) verified that `web-sys` *can* walk the TN
      redirect chain inside a Cloudflare Worker — both `ASP.NET_SessionId`
      and the OTS cookie (`*JPBX=…`, 8-char-uppercase name) are readable
      via `Headers.getAll("set-cookie")` per hop. `reqwest`'s wasm shim
      can't be used: it has no manual-redirect option and auto-follows
      transparently, so we never see the intermediate `Set-Cookie`
      headers. A wasm login implementation will need raw `web-sys` or
      the `worker` crate's `Fetch` — not `reqwest`.

## Parked — wasm-side login

Cron works but requires the laptop to be on and gives no reactive
recovery between ticks. The durable shape is to have the worker
re-authenticate itself. Design sketch (probe results above):

- New crate `tradenation-wasm` inside the `tradenation-api` submodule,
  `#![cfg(target_arch = "wasm32")]`, owns:
  - web-sys redirect-chain login → `tradenation_api::Session`.
  - `TradeNationBroker` (currently in `broker-tradenation`) gains
    credentials + retry-on-`SessionExpired` so a stale cached session
    self-heals in-flight.
- This worker caches the resulting `Session` JSON in KV between
  requests; passes it into the broker at construction; writes it back
  if it changed during a call.
- Drop the `TN_SESSION_JSON` secret + the cron job once it lands.

Pick this up when the cron path proves insufficient — e.g. live trading
at unattended hours, or repeated misses between rotations.

## Done

- **Prep `clears` list to fix stale-ordering bug** — landed. `Intent`
  gains a `clears: Vec<String>` field. The `Prep` handler clears each
  listed prep step before recording the new one; the `Veto` handler
  does the same for vetos (symmetry). Fixes the bug where a stale
  `retest` from before `break-and-close` was satisfied stuck around
  forever and falsely satisfied future ordered gates. CLI gains
  `--clears foo,bar` on the `prep` and `veto` subcommands; the encrypt
  flow prompts for `clears` on prep/veto actions. Recent prep / veto
  names are fuzzy-picked from history (typo-proof) for both the
  `step`/`name` field and the list-of-names prompts. 84 core tests +
  55 cli tests pass; clippy clean.
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
