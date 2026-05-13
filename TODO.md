# TODO

## Done

- **YAML wire format + interactive template-driven encoder** — landed. 58 lib tests passing, clippy clean, non-interactive smoke test passes. Interactive prompts wired through `dialoguer` (gated behind the `cli` feature).

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

### Queryable state (curl-friendly, no HTML)

Add a `GET /state` (or similar) that dumps the current `StateStore` contents as JSON or YAML. Plaintext-curl-able, but authed by the same `ENCRYPTION_KEY` (probably as a query param or header signed with the key). Useful for:
- Confirming a cooldown is in place from another machine.
- Debugging "why didn't my alert fire" without checking CF logs.

### CLI as state-management client

Extend `encrypt-payload` (or rename it `trade-control-cli`) with subcommands that talk to the running webhook:
- `status` — dump active cooldowns + recent `seen` ids.
- `unlock <instrument>` — clear a cooldown manually. Useful when a setup was invalidated by mistake and the user wants to re-arm.
- `list-setups` — once the multi-stage workflow lands, show all setup state machines and their current state.

Auth: the CLI already has the encryption key file; reuse it. The webhook accepts a signed control envelope (the encrypted-intent format with a new `Action::Unlock` / `Action::Status`).
