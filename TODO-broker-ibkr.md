# TODO — Stage 6: `broker-ibkr` + integral sizing

Stage 6 of the IBKR futures integration (plan:
`~/.home-claude/plans/magical-enchanting-bird.md`). Split into **6a / 6b / 6c**
as the plan directs.

Stages 2-5b and 7 are on `feat/contract-calendar`; Stage 1 (`ibkr-client`) is on
the `ibkr-client-promote` worktree, branch `feat/promote-to-client`, **unmerged**.

## What this stage does

Give IBKR an actual `Broker` implementation, so an IBKR account can place a
futures order. This is where the `contract_multiplier` baked in Stage 5 is
finally **consumed**: `contracts = budget / (stop_distance x multiplier x fx)`.

## Where Stage 5 stopped, and what 6b must close

Stage 5 baked `Intent.contract_multiplier`, validated it, and covered it with the
HMAC. But **nothing reads it** — `grep '\.contract_multiplier' core/src/dispatch
engine broker-*` returns zero hits. The field reaches `run_enter` and stops:
`EntryRequest` has no multiplier at all, so it cannot reach a broker's sizing.

That gap is 6b's first task and it is **compile-enforced once the field is added**
to `EntryRequest` (6 construction sites).

## 6a — the `broker-ibkr` crate skeleton — **DONE**

- [x] New workspace member mirroring `broker-oanda/`'s layout: `lib.rs`,
      `ibkr.rs`, `risk.rs`. `risk.rs` **private** (`mod`, not `pub mod`).
      No `fx.rs` yet — see "what is NOT implemented" below.
- [x] Path dep on `../../ibkr-client`. Stage 1 merged to its `main` (`9f8af43`)
      and the directory renamed `ibkr-spike` -> `ibkr-client` to match the crate.
- [x] `IbkrBroker` holding a connected client + account id.
- [x] Verified against the **live paper Gateway** (`127.0.0.1:4002`): all four
      multipliers confirmed (GC 100, MGC 10, ES 50, MES 5), `min_size` and
      `size_increment` both `1` on every listed contract.

## 6b — integral sizing (the multiplier's first consumer) — **DONE**

- [x] `EntryRequest.contract_multiplier: Option<f64>`, threaded from
      `Resolved.contract_multiplier`.
- [x] **The multiplier landed on `Resolved`, not read off the intent per-site.**
      `run_enter` builds **four** `EntryRequest`s (initial + three recovery
      re-placements) and they must all agree — a recovery that re-places on a
      different multiplier silently re-sizes the trade. One field, one source;
      the same drift `pip_size` created by living in both `Intent` and
      `MwParams`.
- [x] `risk::contracts_for_budget(budget, stop_distance, multiplier, fx) -> u32`,
      flooring, returning 0 on any non-finite / <= 0 input.
- [x] `risk::fit_to_size_grid` reads IBKR's own `min_size` / `size_increment`
      rather than a bare `== 0` check, via a named `SizeLimits` pair (two
      adjacent same-typed `f64`s transpose silently).
- [x] `EntryError::ContractSizeUnavailable`, distinct from `UnitsBelowMinimum`:
      the latter is a legitimate small-account outcome that Stage 7 parks and
      re-checks per bar, the former is a plumbing defect no waiting will fix.
- [x] The **`Units` cap check goes through the multiplier too.** A literal size
      on futures means contracts, so omitting it under-reports risk by exactly
      the multiplier — a 2-contract ES order reads as 0.04% instead of 2%.
- [x] `get_quote` fails closed. Market data is not optional: `sl_spread_floor`
      is a hard entry gate reading live spread as a safety signal.

## 6c — wiring — **DONE**

- [x] `BrokerHandle::Ibkr` + **16** cron match arms across 7 files, every one
      compile-enforced. Each arm delegates to the same generic body, so they
      were generated from the TradeNation arm rather than hand-written — arms
      that must stay identical are exactly where hand-writing drifts.
- [x] `Credentials::Ibkr` / `IbkrCreds` — deferred from Stage 4 deliberately,
      because IBKR issues no bearer token. **There is no secret in it**: the
      Gateway process holds the session, so what identifies an account is
      *where the Gateway is* and *which sub-account to trade*. An IBKR
      account's security boundary is the Gateway process and the loopback
      socket, not anything stored here.
- [x] `broker_factory::acquire_ibkr` + `native_cron.rs` + `http.rs`.
- [x] `trade-control-broker-check` gained a real IBKR arm — see below.

### Two decisions worth knowing

**The Gateway address is derived from `kind`, not configured.** Demo ⇒ paper
port 4002, live ⇒ live port 4001. A configurable field would introduce the
failure mode of a live account pointed at paper — or, far worse, the reverse.
This matches how OANDA already picks its host from `kind.is_live()`.

**IBKR reuses the `oanda_account_id` metadata slot.** Not ideal naming, but a
third broker-specific field answering the same question ("which sub-account
under this login") would be worse. `DUR300718` for paper, `U…` for live.

### `broker-check` now checks what it actually can

Stage 4 made it refuse outright. But IBKR's liveness question is the
**connection**, not a quote: the Gateway is a local Java process that
force-restarts daily and re-authenticates weekly, so it is precisely the thing
that breaks — while `get_quote` is unimplemented pending market data. It now
verifies the session and says plainly what it did *not* check.

Two error-message details, both found by running the binary rather than by a
test:

- The "connecting to the IB Gateway…" announcement was printed *before* the
  check that fails without connecting, so a missing account id read as a
  connection problem. Removed.
- `IbkrError::Connect` and `BrokerError::IbkrConnect` both prefixed "ibkr
  gateway connect failed", so the operator saw it twice. The inner type now
  carries only the cause.
- The "is IB Gateway running?" hint is scoped to `IbkrConnect` alone; appending
  it to a config error points the operator at the wrong thing.

## Tests (the correctness anchors)

- [ ] ES multiplier 50: a budget that sizes to 3 contracts must not size to 150.
- [ ] A missing multiplier on a futures instrument ⇒ `ContractSizeUnavailable`,
      **not** a silent `1.0`.
- [ ] Fractional contracts floor (2.9 ⇒ 2), and 0.9 ⇒ 0 ⇒ `UnitsBelowMinimum`
      (which Stage 7 now parks rather than retrying forever).
- [ ] A CFD `EntryRequest` with `contract_multiplier: None` sizes exactly as
      before — byte-identical behaviour for the existing brokers.
- [ ] `min_size` / `size_increment` respected, not just `!= 0`.

## Mutation verification

Per `verify_new_analysis_code_by_mutation` — **10 applied, 10 killed**:

- [x] Multiplier dropped from the sizing division ⇒ 5 tests.
- [x] `.floor()` -> `.round()` (the over-risking direction) ⇒ 3 tests.
- [x] Missing multiplier defaulted to `1.0` ⇒ 2 tests.
- [x] `size_increment` ignored ⇒ `a_stepped_increment_floors_onto_the_grid`.
- [x] `min_size` ignored ⇒ `a_size_below_the_exchange_minimum_is_refused`.
- [x] Multiplier dropped from the `Units` implied-risk cap ⇒ its test.
- [x] Multiplier checked **after** the account fetch ⇒ **initially SURVIVED**.
      The ordering had no coverage; `a_missing_multiplier_is_refused_before_the_account_is_consulted`
      was written and it now dies. See below.
- [x] Zero size mapped to `ContractSizeUnavailable` ⇒ 2 tests.
- [x] The size-grid fit skipped entirely ⇒ its test.
- [x] Open-positions cap `>=` -> `>` ⇒ its test.
- [x] Paper and live Gateway ports swapped ⇒ `the_gateway_port_follows_the_account_kind`.
- [x] Missing IBKR account id defaulted instead of refused ⇒ its test.
- [x] Client-id floor dropped ⇒ `the_client_id_floor_holds_at_the_extremes`.
- [x] Client-id span narrowed back to 60k ⇒ `ibkr_client_ids_do_not_collide_across_many_accounts`.

### A mutation that found a real bug, not a missing test

Removing the client-id floor initially survived, so the test was widened to
sweep 2,000 generated names — **and that sweep went red on the collision
check**: hashing into a 60,000-wide range collides across a few hundred
accounts (the birthday paradox), and a duplicate client id is rejected by the
Gateway outright, so the second account simply cannot connect while the first
is up. An outage that only appears once both are live. The range is now the
full positive `i32` above the floor.

The widened sweep still could not kill the floor mutation — over a range that
wide, random names never land near the floor, so it asserted a guarantee it had
no power to check. The mapping is now a pure `client_id_from_hash` tested at
`0`, `1` and `u64::MAX`, where the floor is actually falsifiable.

### The one that got away first time

Reordering the multiplier check to *after* the account fetch passed every test.
The ordering is not cosmetic: a Gateway that is merely down would then mask a
missing multiplier, and the operator would see `AccountFetch` and chase a
connection problem while the real fault is an intent armed without a multiplier.

Closing it needed `place_entry` to be callable offline, so the Gateway read —
the part that is genuinely unimplemented — became a named `AccountSource` seam.
That is not test scaffolding: it separates the money math (complete) from the
I/O (not), and lets the real implementation drop in without touching sizing.
The test asserts the account was **not consulted**, with a mirror test proving a
well-formed request still reaches it (or "never fetch" would satisfy it
trivially).

## Open risks (external, per the plan)

Stage 6 is where the plan says the real risk lives, and it is **not
architectural**:

- **Market-data entitlements** — live vs delayed COMEX/CME quotes. Binary,
  gating, costs money. *"The most likely thing to stop the project."*
- **The order path has never been exercised** — the spike is read-only and has
  never placed an order.
- **IBC is not installed**, so the daily-restart / weekly-reauth cycle is
  unproven over 48h+.

The paper Gateway **is** currently up on `127.0.0.1:4002`, so 6a can be verified
against a live connection rather than only unit tests.

## What is NOT implemented, deliberately

Sizing is complete; **transmission is not**. Every operation that would send to,
or read live state from, the Gateway returns a loud failure rather than a
plausible empty answer:

| operation | returns | why not a stub |
|---|---|---|
| order submission | `OrderRejected` + `error!` | a fabricated order id would be recorded as a real live order |
| `account_snapshot` | `AccountFetch` | sizing against an invented equity is worse than not sizing |
| `close_positions` | `Errored` (**not** `NothingOpen`) | `NothingOpen` is a *success* since v135 — it consumes the intent id and would mark a close fulfilled while a real position ran |
| `list_open_positions` | `Transient` | an empty `Vec` reads as "the account is flat"; the breakeven watch, pending sweep and blackout apply all act on that |
| `lookup_attempt_state` | `Transient` (**not** `Unknown`) | `Unknown` tells the retry gate "this attempt is dead, place another" — it would re-place over an order it cannot see |
| `get_quote` | `Transient` | market-data entitlement unconfirmed; a delayed quote silently defeats the SL-spread floor |

`no_silent_degrade_prefer_loud_failure` applies with unusual force to a broker
adapter: the caller cannot tell a polite lie from the truth.

## A constraint found in `ibapi`, recorded before it bites

`BracketOrderBuilder` offers `entry_market()` and `entry_limit()` and **no
`entry_stop()`** — but this system's primary entry mode is a *stop* entry above
/ below the neckline. So a stop entry cannot be placed as one bracket; it needs
a parent stop order with SL/TP children attached by `parent_id`.

Pinned as `bracket_can_carry` + `only_market_and_limit_entries_fit_a_bracket` so
the order path starts from the constraint rather than rediscovering it against a
live Gateway.

## Verified against real binaries, against the LIVE paper Gateway

Not just unit tests. The Gateway was up on `127.0.0.1:4002` throughout:

- `trade-control-accounts add --broker ibkr --kind demo --oanda-account-id DUR300718 ibkr-paper`
  ⇒ stored, and `list` reads it back as `ibkr`.
- `trade-control-broker-check ibkr-paper` ⇒ **`OK — IB Gateway session is
  live`**. The whole path works end to end: account row → `BrokerKind::Ibkr` →
  `acquire_ibkr` → derived paper port → hashed client id → a real Gateway
  session.
- An IBKR account with no account id ⇒ refused *before* any socket work, with
  a message naming the actual fault.
- A `--kind live` account ⇒ routed to port 4001 (nothing listening) ⇒
  `Connection refused` **plus** the Gateway hint. This also proves the
  demo/live port derivation, which no unit test can.
- The account id's `--help` said "ignored for TradeNation" and did not mention
  IBKR needs it. Corrected.

`ibkr-paper` (`DUR300718`) is left configured — it is a working paper account
Stage 9 needs, and nothing can place an order through it yet.

## Status: 6a + 6b + 6c DONE — Stage 6 complete
