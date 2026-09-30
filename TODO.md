# TODO — OANDA order→trade id bridging + reconciliation cron (2026-09-30)

## Incident

Plan `hs-aud-jpy-dd5db625` (account `m-and-w`, staging, OANDA practice
`101-011-31142393-003`). `05-enter` placed a `STOP` order (id `2494`), which
filled into trade `2495`, which later hit its TP and closed
(`realizedPL: 18369.2245`, closed `2026-09-30T01:40:25Z`, confirmed via raw
OANDA API). The system never noticed: a same-plan refire at 03:00 got
`rejected: prior-attempt-unknown`, and the plan kept ticking pause/news/veto
rules against a position that had already filled (and later closed) for over
2 hours, with the account fully flat the whole time.

## Root cause (confirmed against live OANDA data + code)

`broker-oanda/src/oanda.rs` assumes, in (at least) two places, that **OANDA
reuses the create-order transaction id as the trade id**. That's only true
for a market order that fills immediately in the same request. For a
resting `STOP`/`LIMIT` order, the fill is a *separate* transaction with its
own id (`fillingTransactionID`), and the resulting trade gets that fill
transaction's id — **not** the original order's id.

Verified against real data for this incident:
- order **2494** (STOP) → `fillingTransactionID: "2495"`, `tradeOpenedID: "2495"`
- the trade's actual id is **2495**, not 2494

Affected spots:
1. `compute_attempt_state` step 2 (`oanda.rs:716-725`) — matches
   `trade.id == broker_order_id`. Wrong for a stop/limit fill: the real
   trade id is the *order's* `filling_transaction_id`, not the order id
   itself. Comment at line 717-720 states the false assumption explicitly.
2. `list_open_positions` (doc comment ~line 525) — "OANDA has no separate
   originating-order id once a trade is open, so `order_id` and
   `position_id` both carry the trade id." Same false assumption, second
   location — check call sites before deciding whether it's a live bug or
   just a stale comment.

Knock-on effects of (1):
- Step 2 never matches while the trade is genuinely open → `broker_trade_id`
  is never snapshotted onto the `EntryAttempt` row
  (`retry_gate.rs:275-287`, only set from `AttemptState::OpenPosition`)
- Without a snapshotted `broker_trade_id`, step 3 (closed-trade check) is
  skipped entirely (`oanda.rs:395`, "step 3 only runs in that case")
- Falls through to the fate lookup (`fetch_order_fate`, already calls
  `get_order` which *does* return `filling_transaction_id` — the bridge
  data was one field away the whole time) → `Unresolved`/`Live` →
  `AttemptState::Unknown`
- Retry gate fail-safes on `Unknown` (correct behavior *given* the bad
  input) → `prior-attempt-unknown`, and nothing downstream (sweep,
  breakeven watch, blackout) can see this plan either, since all of them
  are keyed off resolvable attempts.

Distinct from `BUG-cancelled-limit-order-resolves-unknown-blocks-reentry.md`
(a different case: order cancelled *before* ever filling — already fixed by
the `OrderFate` fate-lookup step). This bug is about an order that *did*
fill and later closed, misclassified because of the id-bridging assumption.

## Plan (small, sequential changes — test each before moving on)

- [x] **1. Logging** — `compute_attempt_state`'s fallthrough now logs the
      actual `open`/`closed` trade ids seen alongside `broker_order_id` and
      the fate's `filled_trade_id` whenever it resolves to `Unknown`, so a
      future occurrence is diagnosable from worker logs alone (this incident
      needed a raw OANDA API pull to diagnose — landed as part of item 2,
      same commit, since the new bridging logic and its logging are one
      change).
- [x] **2. Fix the order→trade bridge** — `OrderFate::Live` now carries
      `filled_trade_id: Option<String>`, populated from
      `OrderDetails::filling_transaction_id` in `fetch_order_fate`.
      `compute_attempt_state` gained step 2.5: when the fate bridges to a
      trade id different from the order id, re-check `open`/`closed` against
      it before falling through to `Unknown`. `lookup_attempt_state` fetches
      closed trades on a second pass if the bridge reveals an id and none
      were fetched yet (no pre-snapshotted `broker_trade_id`). Tests added
      (`broker-oanda/src/oanda.rs`, `attempt_state_tests` module):
      - `live_fate_with_bridged_id_matching_nothing_resolves_unknown` —
        bridge present but matches nothing → still fail-safe `Unknown`
      - `bridged_trade_id_resolves_open_position_when_order_id_differs_from_trade_id`
      - `bridged_trade_id_resolves_closed_win_when_order_id_differs_from_trade_id`
        — pinned to this incident's exact ids/P&L (2494→2495, +18369.2245)
      - all pre-existing tests still pass unchanged (49/49 in `broker-oanda`)
      - conformance suite (`cli/src/bin/replay_candles/attempt_state_conformance.rs`)
        updated for the new `OrderFate::Live` field shape (`filled_trade_id: None`,
        its existing fixtures don't exercise the bridge); 5/5 still pass
      - `list_open_positions`'s doc-comment assumption ("no separate
        originating-order id once a trade is open") is the SAME false
        assumption in a second location — checked call sites, not fixed here:
        that function only reports what's open *right now* (a live sweep),
        it doesn't need to bridge order id → trade id for a *closed* trade,
        so it's lower risk, but the comment is still wrong and worth a
        follow-up pass if a similar bridging need surfaces there.
      - cargo clippy + fmt clean on `broker-oanda` + `trade-control-cli`
- [x] **3. Reconciliation cron** — new module
      `trade-control-cron/src/reconcile.rs`, `pub async fn reconcile<S:
      StateStore, C: CronEnv>`. Walks every `EntryAttempt` with a
      snapshotted `broker_trade_id` (i.e. reached an open position at some
      point), groups by account, fetches `Broker::list_open_positions` per
      account (broker-truth, not attempt-keyed), and for any tracked trade
      id absent from that snapshot, resolves the real state via
      `Broker::lookup_attempt_state` and logs the mismatch (`ClosedWin` /
      `ClosedLossOrBreakeven` with realized P&L, or any other
      non-open-non-resolvable state) at `warn!`. **Observation-only** —
      touches no `EntryAttempt`/plan state, never closes/cancels anything,
      per the operator's explicit instruction (2026-09-30: "we should be
      fine not marking it; I want to see what happens; it's just a demo
      account" — watch behavior first before deciding whether/how to
      auto-retire a plan).
      - Wired into `worker/src/scheduler.rs` as its own `reconcile_loop`,
        same `skip_interval` + `run_isolated` panic-containment pattern as
        every other cron pass, on its own `upkeep_interval`-cadence timer —
        fully independent of the engine tick/sweep/order-control passes, so
        a broker error or slow tick here can never affect real trading.
      - Tests (`trade-control-cron/src/reconcile.rs`, via a seamed
        `AttemptBroker` trait mirroring `breakeven_watch`'s `PositionBroker`
        pattern so the decision is testable without a live broker):
        - `still_open_at_broker_is_a_noop` — position present in the
          broker's open-positions snapshot → no broker lookup call at all
        - `closed_win_absent_from_open_positions_is_detected` — exact
          incident shape (absent from open, resolves `ClosedWin`)
        - `no_tracked_attempts_short_circuits_without_a_broker_call` —
          an attempt with no `broker_trade_id` is excluded upstream
      - 66/66 `trade-control-cron` tests pass, 33/33 `trade-control-worker`
        tests pass, clippy + fmt clean on both.
      - Deliberately did NOT add a mismatch between `list_open_positions`
        and a concurrent `lookup_attempt_state` race as a hard error — see
        the `AttemptState::OpenPosition` arm in `reconcile_one`, logged at
        `info!` and left to the next tick, since the two broker calls are
        not atomic with each other.

## Notes

- cargo clippy + cargo fmt gate before each commit, per repo conventions.
- Each of the 3 items above should be its own commit; push after each
  green + clippy/fmt-clean step per the commit-and-push policy.
- This is a `fix/oanda-order-trade-id-bridge` worktree branched off `main`
  (bug fix scope), sibling of the crates it path-deps on
  (`oanda-client` at `../../oanda-client`).

---

# TODO — register rejects a plan whose broker disagrees with its account (2026-09-28)

`hs-aud-usd-d6ca1271` / `hs-btc-usd-755e77b3` were armed `broker: oanda` on the
TradeNation `experimental` account. Register said 200; the engine then fetched
`AUD_USD` from TradeNation every 15s, failed "transient", never seeded state, and
no rule (not even `trade-expiry`) ever fired.

- [x] `trade_plan::broker_check`: pure check — every rule's `intent.broker` == the account's broker
- [x] `handle_register` takes the resolved account and 400s a mismatch (names the rules)
- [x] worker `http.rs`: resolve the named account before register; unknown account → 400
- [x] tests, clippy, fmt
- [x] deploy staging; verified: re-registering the AUD plan 400s naming all 10 OANDA rules
- [ ] follow-up (separate): `MarketUnavailable` is permanent, not `Transient`
- [ ] follow-up (separate): time rules / expiry can't fire on a plan that never seeded

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
