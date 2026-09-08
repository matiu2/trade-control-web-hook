# TODO — Stage 4: `BrokerKind::Ibkr` + the parallel-enum sweep

Stage 4 of the IBKR futures integration (plan:
`~/.home-claude/plans/magical-enchanting-bird.md`).

Stage 2 (calendar + generator) is `9d43289`; Stage 3 (the arm-time gate) is
`f9034ef`. Both on `feat/contract-calendar`, worktree
`../trade-control-contract-calendar`.

## What this stage does

Add the `Ibkr` variant to the broker enums **without** a broker
implementation, so the compiler enumerates the remaining work. This is also the
stage that makes the operator's four binaries IBKR-aware:
`trade-control-accounts`, `tv-arm-staging`, `trade-control-staging`,
`journal-staging`.

No order can be placed at the end of this stage — `broker_factory` has no IBKR
arm, so an IBKR account resolves to a loud "not implemented" error rather than
a wrong broker.

## Survey — what the plan said vs. what is actually there

236 `BrokerKind` references across 41 files. Most are test fixtures carrying
`broker: oanda`; those need no change (`#[default] Oanda` covers them).

### Compile-enforced (the compiler will list these)

- `core/src/intent.rs` — `BrokerKind` itself.
- `core/src/account/creds.rs` — `Credentials` (tagged by broker).
- `core/src/account/store.rs` — `resolve`'s creds→kind match.
- `cli/src/admin_secret.rs` — secret-name prefix.
- `cli/src/calendar_bars.rs` — `BrokerKind` → `instrument_lookup::Broker`.
- `cli/src/instruments.rs` — instrument validation.
- `cli/src/bin/trade_control.rs` — 3 account subcommand matches + clap mirror.
- `tv-arm/src/broker_kind.rs` — `Broker` ↔ `BrokerKind` bijection.
- `worker/src/native_cron.rs`, `worker/src/http.rs` — broker acquisition.
- `worker/src/bin/accounts.rs`, `worker/src/bin/broker_check.rs` — display + clap.
- `conventions/src/broker.rs` — `Broker` (`from_exchange`/`from_wire`/`as_str`/
  `default_account_index`).
- `tv-arm/src/args.rs` — `BrokerArg`. **A fifth mirror the plan did not list.**
- `tv-arm/src/replay.rs:146` — `Broker` → `CandleSource`.
- `tv-arm/src/instrument_resolution.rs:195` — `ConvBroker` → `IlBroker`.

### Silent risks (NOT compile-enforced — the real work)

Line numbers below are as surveyed *before* the change; several moved.

- `worker/src/broker_factory.rs:77,105` — `!=` guards, not matches.
- `worker/src/pg_accounts.rs:55` — `unwrap_or_else(|| "oanda")` on a serde
  failure. A broker that fails to serialise is silently stored as OANDA.
- `cli/src/trade_patterns.rs:1878` — `_ => BrokerKind::TradeNation` catch-all
  on a menu index. Adding a third menu entry silently maps it to TradeNation.
- `cli/src/interactive.rs:703`, `cli/src/trade_patterns.rs:1869` — hardcoded
  `["oanda", "tradenation"]` menu arrays.

### Sites the plan did not list, found by the compiler

- `tv-arm/src/args.rs` — a **fifth** broker mirror.
- `tv-arm/src/spread.rs` — the live bid/ask read (×2, now unified into one
  `read_bid_ask`). Matters: the SL-spread-floor is a hard entry gate.
- `conventions/src/instrument.rs` — FX symbol reshaping. A 6-character futures
  root would have been split 3+3 into a currency pair.
- `tv-arm/src/pipeline.rs` — chart-symbol recovery, and `resolve_account`.
- `journal/src/tv.rs` — string-keyed, so it compiled; documented rather than
  fixed (a futures chart's exchange is per-contract: COMEX vs CME).

### Deliberately NOT given an `Ibkr` variant

Three of the "four parallel enums" in the plan should not get one — a dead
variant every future reader must handle is worse than no variant:

- **`cli/src/replay_args.rs` `CandleSource`** — names a *candle-cache source*.
  There is no IBKR candle-cache feed. `tv-arm/src/replay.rs:146` maps
  `Broker → CandleSource`, so this becomes a real decision the compiler forces:
  IBKR must map to an explicit refusal, not a silent OANDA fallback.
- **`spread-baseline-gen/src/lib.rs` `Broker`** — generates CFD *spread*
  profiles from broker tick data. Futures have no CFD spread profile.
- **`market-hours-gen` `Venue`** — generates CFD session tables. CME/COMEX
  session hours are a different model and are not sourced from these venues.

Recorded here rather than silently skipped, because the plan explicitly lists
them and a later reader will otherwise think they were missed.

`instrument-lookup`'s own `Broker` is out of scope: it is a **separate
submodule** and Stage 5's concern (it needs `AssetClass::Future` + rows too).

## Tasks

- [x] `BrokerKind::Ibkr` + `BrokerKind::ALL` + keep `#[default] Oanda`.
- [x] `conventions::Broker::Ibkr` + its methods (+ `Broker::ALL`).
- [x] Drive the hardcoded menu arrays from `BrokerKind::ALL`.
- [x] Kill the `_ => TradeNation` catch-all.
- [x] `pg_accounts` serde failure ⇒ error, not `"oanda"`.
- [x] Narrow the Stage 3 close-out guard with `broker == Ibkr`.
- [x] Correct `broker_factory`'s module doc re: monomorphization.
- [x] Three clap mirrors accept `ibkr` (`accounts`, `trade-control`, `tv-arm`).
- [~] `Credentials::Ibkr` — **not done, deliberately.** See below.
- [~] `broker_factory` `!=` guards — **left as `!=`, deliberately.** See below.

## Tests (the correctness anchors)

- [x] `#[default]` is still `Oanda` — a wire body with no `broker:` field must
      still deserialise to OANDA (pre-feature compat).
- [x] `ibkr` round-trips through serde on `BrokerKind`, and `as_str` agrees with
      serde for every variant (the account row is *written* with `as_str` and
      *read* through serde, so a disagreement changes an account's broker).
- [x] `Broker::from_wire`/`as_str` round-trip covers every variant via `ALL`.
- [x] Each `acquire_*` rejects every foreign broker, driven off `ALL`.
- [x] The close-out guard fires for a futures symbol on **any** broker, and an
      IBKR plan naming a non-futures instrument is refused too.
- [x] A CFD plan on a CFD broker is untouched.
- [x] IBKR has no default account and reports `None` rather than a placeholder.
- [x] IBKR symbols pass through `instrument_for` verbatim.
- [x] IBKR has no `CandleSource`.
- [x] Every clap mirror maps `ibkr` to its own variant (not merely parses it).

## Two tasks deliberately NOT done

**`Credentials::Ibkr` + `IbkrCreds` — deferred to Stage 6.** `Credentials` is a
serde-tagged enum whose variants carry *the fields needed to authenticate*. IBKR
does not authenticate with a token or a password: the Gateway holds the session
and the client connects to a local socket. Inventing a credential shape now
would bake a guess into a signed, serialised type before the broker exists to
say what it needs. `store.rs::resolve` matches creds→kind exhaustively, so the
variant lands with the code that can fill it. Until then an IBKR account has
metadata but no credentials — which is exactly right, because nothing can
connect yet.

**`broker_factory`'s `!=` guards stay `!=`.** The plan called for exhaustive
matches, on the theory that `!=` is not compile-enforced. Reading them, each
guard rejects *every* broker other than its own — including ones added later —
so the failure mode the plan feared (a new broker silently acquiring the wrong
client) cannot occur. Converting them would be churn with no behaviour change.
`each_factory_rejects_every_foreign_broker` walks `BrokerKind::ALL` and asserts
it, so the property is now tested rather than assumed, and a future broker is
covered with no new test to remember.

## Mutation verification

Per `verify_new_analysis_code_by_mutation` — all six applied and **killed**:

- [x] `#[default]` moved to `Ibkr` ⇒ `an_absent_broker_field_still_means_oanda`.
- [x] `BrokerKind::ALL` omits `Ibkr` ⇒ `all_lists_every_variant` **and** the
      cross-crate `every_broker_round_trips` (two independent tests).
- [x] `acquire_oanda`'s guard dropped ⇒ `each_factory_rejects_every_foreign_broker`.
- [x] Guard scoped as `is_futures && broker == Ibkr` (the trap the docs warn
      about) ⇒ **six** tests, led by `a_futures_symbol_is_guarded_even_on_a_cfd_broker`.
- [x] IBKR + non-futures instrument allowed through ⇒
      `an_ibkr_plan_naming_a_non_futures_instrument_is_refused`.
- [x] IBKR given a placeholder default account ⇒
      `ibkr_has_no_default_account_and_says_so`.
- [x] IBKR symbol reshaped like a CFD pair ⇒ `ibkr_symbols_pass_through_verbatim`.

## Verified against real binaries

Not just unit tests — the actual CLIs, with a temporary account created and
removed:

- `trade-control-accounts add --broker ibkr` ⇒ stored, and `list` reads it back
  as `ibkr` (proves the `broker_to_str` rewrite round-trips through Postgres).
- `trade-control-broker-check <acct>` ⇒ refuses with a clean one-line message.
- `trade-control instruments list --broker ibkr` ⇒ `no instrument catalog for ibkr`.
- `--help` shows `[possible values: oanda, tradenation, ibkr]`.

## Gate

- 2873 workspace tests green, 0 failures.
- `cargo clippy --workspace --all-targets` — no warnings in any touched file
  (the remaining set is pre-existing: `engine/`, `market-hours-gen/`,
  `core/src/signals/state_machine.rs`, two `cli/` files).
- `cargo fmt --all --check` clean.

⚠️ **`cargo fmt` collapses a `\` string continuation into literal spaces** when
the line fits after reflowing. Two operator-facing messages were left reading
`...yet —                  nothing to check`. Both fixed and re-verified by
running the binary. Worth re-checking any multi-line `eyre!` after a fmt.

## Status: COMPLETE

Next is Stage 5 (`contract_multiplier` baked onto the signed intent), which also
carries the `instrument-lookup` work — `Broker::Ibkr`, `AssetClass::Future` and
contract rows — in that submodule.
