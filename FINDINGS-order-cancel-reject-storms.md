# FINDINGS — `ORDER_CANCEL_REJECT: ORDER_DOESNT_EXIST` storms (Plan Stage 7)

**Status: DIAGNOSED, not fixed.** The mechanism is proven from code, and it is
**not** the root the Stage 1 agent hypothesised. No speculative fix applied, per
the plan's instruction. A proposed fix is at the bottom, deliberately not
implemented.

Companion to `BROKER-EVIDENCE-trade-142-eurcad.md` §6 and
`PLAN-trade-142-parity-and-order-loss.md` Stage 7.

## The evidence being explained

```
2300  2026-07-24T15:51:44Z  ORDER_CANCEL_REJECT  orderID=2291  ORDER_DOESNT_EXIST
2301  2026-07-24T15:51:45Z  ORDER_CANCEL_REJECT  orderID=2291  ORDER_DOESNT_EXIST
2310  2026-07-26T22:50:07Z  ORDER_CANCEL_REJECT  orderID=2270  ORDER_DOESNT_EXIST
2311  2026-07-26T22:50:08Z  ORDER_CANCEL_REJECT  orderID=2270  ORDER_DOESNT_EXIST
2312  2026-07-26T22:50:09Z  ORDER_CANCEL_REJECT  orderID=2270  ORDER_DOESNT_EXIST
2313  2026-07-27T00:45:56Z  ORDER_CANCEL_REJECT  orderID=2302  ORDER_DOESNT_EXIST
2314  2026-07-27T00:45:58Z  ORDER_CANCEL_REJECT  orderID=2302  ORDER_DOESNT_EXIST
2315  2026-07-27T00:45:59Z  ORDER_CANCEL_REJECT  orderID=2302  ORDER_DOESNT_EXIST
```

Two questions, with different answers. **Keep them apart** — conflating them is
what makes this look like one bug.

1. **Why is a cancel issued against a dead order at all?** (the *first* row of
   each burst)
2. **Why 2-3 of them, one second apart?** (the *repeats*)

## Q2 first — the burst is ONE call, retried inside `oanda-client`

This is the load-bearing finding, and it makes the storms far less alarming than
they look: **each burst is a single `Broker::cancel_order` call**, not 2-3
decisions by our system.

`broker-oanda`'s `cancel_order_impl` calls `oanda_client::cancel_order`, which
wraps the HTTP PUT in `execute_request_with_retry("cancel_order", true, …)`
(`oanda-client/src/orders.rs:810`). The `true` selects the **conservative
trading** retry policy (`oanda-client/src/retry.rs:61`):

```rust
max_attempts: 2,                     // loop is `for attempt in 0..=max_attempts`
base_delay: Duration::from_secs(1),  //   => 3 TOTAL attempts
backoff_multiplier: 1.5,             // delays: 1.0s, then 1.5s
use_jitter: true, jitter_factor: 0.1,
retry_on_connection_error: true,
```

Delay for attempt *n* is `base × 1.5^n` ± 10% jitter ⇒ **1.0s then 1.5s**. That
is exactly the observed spacing (`22:50:07 / :08 / :09` and
`00:45:56 / :58 / :59`), and exactly the observed *count* (3, or 2 when the
overall future was dropped first).

### Why a 404 is retried at all — the classifier's default arm

`ORDER_DOESNT_EXIST` comes back as an HTTP **404**, and `cancel_order` renders it
as a plain `eyre!` string: `"Cancel order failed with status 404 Not Found: …"`
(`oanda-client/src/orders.rs:831`). `OandaClient::classify_error`
(`oanda-client/src/lib.rs:293`) then string-matches that message:

* not `401`/`403`/`Unauthorized`/`Forbidden` → not `Authentication`
* not `429` → not `RateLimit`
* not `500`/`502`/`503`/`504` → not `Server`
* not `connection`/`timeout`/`network`/`dns` → not `Network` *by that arm*
* not `duplicate`/`invalid order`/`insufficient funds` → not `Trading`
* not `400`/`Bad Request` → not `Permanent`
* **falls through to the final default: `RetryError::Network(msg)`**

and `RetryError::Network` is retryable whenever `retry_on_connection_error`,
which the conservative config sets `true` (`retry.rs:114`, `retry.rs:69`).

**So a permanent, semantically-final 404 is misclassified as a transient network
blip and retried three times.** Note `404` is never tested for, and the
`400..=499` arm of `is_retryable` (which correctly returns `false` for a 404)
is only reachable via `RetryError::Http`, a variant `classify_error` **never
constructs**. The status code is available at the call site and is discarded
into a string before anything can act on it.

This is a defect in **`oanda-client`** (a separate crate/repo), not in
`trade-control-web-hook`. It costs two wasted round-trips and two spurious
broker-side rejection transactions per dead-order cancel. It is *not* a
correctness bug for us: `broker-oanda`'s `cancel_order` (`oanda.rs:475`) maps the final failure to
`CancelError::Transient` either way, so our own logic sees one failure.

## Q1 — why a dead order is targeted: STALE STORED IDs, and it is NOT one root

Four call sites reach `Broker::cancel_order` / `cancel_pending_for_instrument`.
They split cleanly by **where the order id comes from**, and only one half can
ever produce `ORDER_DOESNT_EXIST`:

| Caller | Id source | Can go stale? |
|---|---|---|
| `dispatch::veto` / `dispatch::invalidate` → `cancel_pending_for_instrument` | **live** `get_pending_orders` listing, filtered by instrument (`broker-oanda/src/oanda.rs:103`) | **No** — it only cancels what the broker just said is resting |
| `pending_lifecycle::cancel_pass` → `try_cancel_one` | **live** `list_pending_orders` (`pending_lifecycle.rs:396`) | **Not on the first attempt** — but see below |
| `order_control` reprice/demote | **live** `list_pending_orders` (`trade-control-cron/src/order_control_tick.rs:241`) | No |
| `retry_gate::evaluate`, `AttemptState::Pending` arm (`retry_gate.rs:239`) | **stored** `EntryAttempt.broker_order_id` | **Yes**, but gated — see below |
| `cron sweep` `cancel_with_broker` (`trade-control-cron/src/sweep.rs:262`) | **stored** `EntryAttempt.broker_order_id` | **Yes — unguarded** |

### The retry gate is NOT the source (refutes the Stage 1 hypothesis)

The Stage 1 agent hypothesised these share a root with the bug `2c86afa` fixed.
**They do not, and `2c86afa` does not reduce them.**

`2c86afa` changed *when* the retry gate runs relative to the other gates — it
did not change *which* orders it cancels. Its cancel arm is reached only after
`lookup_attempt_state` returns `Pending` for that specific attempt
(`retry_gate.rs:236-239`). `Pending` means the broker **just confirmed the order
is resting**, so a cancel that immediately follows cannot get
`ORDER_DOESNT_EXIST` except in a genuine sub-second fill race — and the code
already handles exactly that, by re-looking-up and returning
`rejected: raced-with-cancel` (`retry_gate.rs:245-268`).

Two further facts rule it out on the evidence itself:

* **Cadence.** The engine tick is **15s** and upkeep is **900s**
  (`~/.config/trade-control/{local,staging}-worker.toml`; defaults 60s/900s in
  `worker/src/config.rs`). *No* scheduler in this system can produce two
  decisions **one second apart**. Only an in-call retry can, which is Q2.
* **Order 2302 filled** (as trade 2303) and was still cancel-attempted. A filled
  order resolves `OpenPosition` / `ClosedWin` / `ClosedLossOrBreakeven`, none of
  which reach the gate's cancel arm at all.

### The two real sources

**(a) `cron sweep` — stored id, no state check.** `sweep_one`
(`trade-control-cron/src/sweep.rs:100-110`) cancels on `expires_at`,
`cancel_at`, or SL-breach using `attempt.broker_order_id` **without ever asking
whether the order is still resting**. If the order filled (2302→2303) or was
cancelled out-of-band earlier, the row is still there and the sweep cancels a
dead id. This fits order 2302 exactly: it filled on 07-25, and the sweep's
expiry branch fired on 07-27, two days later.

It is also **fire-once**: `cancel_and_delete` calls `cancel_with_broker` (which
swallows the error and returns `()`) and then `delete_row` **unconditionally**,
so the row is gone regardless of outcome. That is a second, separate defect —
the log line says *"will retry next tick"* (`sweep.rs:273`) but the row it would
retry from has just been deleted. It also means the sweep contributes **one**
burst per attempt, never a repeat, which is consistent with the evidence once
Q2 explains the repeats.

**(b) `pending_lifecycle`, pre-v121 — stored id, retried every tick.** Before
`374ae98` (**2026-07-30**, v121), a failed cancel was *swallowed*: the log said
"record stays" and the `CancelledOrder` entry was **kept**, so the next
lifecycle tick re-cancelled the same id forever. All three storm dates
(**07-24, 07-26, 07-27**) predate that fix.

`374ae98` is therefore the commit that matters here, not `2c86afa`. It added
`classify_cancel_failure`, which on a failed cancel re-looks-up and, for every
non-`Pending` state, classifies `Vanished` and **prunes the entry from the
record**. A dead order is now cancelled at most once per hold episode instead of
once per tick.

## Verdict

| Question | Answer |
|---|---|
| Same root cause as the trade-142 order loss? | **No.** That was gate *ordering*; this is *stale stored ids* + a *misclassified 404*. Disjoint. |
| Fixed by `2c86afa`? | **No.** It reordered gates; it changed no cancel target. The retry gate was never the source. |
| Fixed by anything? | **Partly.** `374ae98` (v121, 2026-07-30) removed the per-tick repetition from `pending_lifecycle`, and postdates every storm in the evidence. |
| Residual bug? | **Yes, two.** Both below. |

### Residual 1 — `oanda-client` retries a permanent 404 (the burst)

A 404 is classified `RetryError::Network` by the classifier's fall-through
default and retried 3× at ~1s. Every `ORDER_DOESNT_EXIST` therefore costs three
transactions instead of one, and any *other* permanent 4xx is equally affected.

**Proposed fix (NOT implemented — different repo, and it changes retry behaviour
for every OANDA call in the fleet):** have `classify_error` see the status code
rather than a formatted string. Either construct `RetryError::Http(status, msg)`
at the call sites — the `400..=499` arm of `is_retryable` already returns
`false` for a 404 — or, as a smaller change, add a `404`/`Not Found` test to
`classify_error` returning `RetryError::Permanent`. **Do not** simply flip the
final default to `Permanent`: genuine unclassified transients would stop being
retried, which is a worse failure on the money path. Wants its own tests in
`oanda-client` asserting a 404 is not retried and a connection error still is.

### Residual 2 — `cron sweep` cancels a stored id without checking its state

`sweep_one` cancels `attempt.broker_order_id` on expiry / bar-expiry /
SL-breach with no `lookup_attempt_state` first, so a filled or already-cancelled
order is cancel-attempted anyway. This is the *first* row of a sweep-driven
burst, and it is the only remaining unguarded stale-id path now that v121 has
fixed `pending_lifecycle`.

Harmless at the broker (the cancel is rejected, nothing is destroyed) but it
generates the failure transactions and, more importantly, **the sweep deletes the
row regardless of outcome** while logging "will retry next tick" — so a
genuinely-transient cancel failure loses its retry AND its tracking row.

**Proposed fix (NOT implemented — needs its own tests and is not certain enough
to bundle with Stage 6):**

1. In `cancel_with_broker`, return the outcome instead of `()`, and have
   `cancel_and_delete` delete the row only when the cancel succeeded **or** a
   re-lookup says the order is gone — the `CancelOutcome` shape
   `pending_lifecycle` already uses. That fixes the "retry next tick" lie.
2. Optionally short-circuit: skip the cancel entirely when
   `lookup_attempt_state` is not `Pending`. This costs a broker round-trip per
   swept row, so it is a real trade-off, not a free win — worth measuring
   before adopting.

Reusing `pending_lifecycle::CancelOutcome` / `classify_cancel_failure` is the
obvious route; both are already generic over `Broker`.

## Not investigated

Whether orders 2291 and 2270 were filled or out-of-band-cancelled before their
bursts. `BROKER-EVIDENCE` §6 confirms it only for 2302 (filled as trade 2303).
Establishing it for the other two would need another `list_transactions` pull
and would confirm, not change, the diagnosis above.
