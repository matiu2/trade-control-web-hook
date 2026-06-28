# Test task: verify `amend_stop` on an OPEN TradeNation position (demo)

**For:** another LLM/agent to execute on **Monday** (markets are closed over the
weekend, so positions can't be opened until then).
**Account:** the **experimental** TradeNation demo account (never live money).
**Branch under test:** `feat/breakeven-stop-50pct` (worktree
`~/projects/trading-libraries/trade-control-web-hook-be-stop`).
**Time budget:** ~20–30 min once a market is open.

---

## Why this test exists

Two new (unmerged, undeployed) features move the stop-loss of an **already-open**
position by calling the broker's `amend_stop`:

1. **Break-even stop** (this branch) — moves the SL to the entry price once a
   candle closes past 50%-to-TP.
2. **Spread-blackout stop-widen** (already in the tree) — widens the SL away
   from price around the NY-close spread blowout.

Both call the same TradeNation adapter method, and that method is **UNVERIFIED**:
the upstream `amend_order` (`AmendCloseOrder`) has had **no callers ever**. It is
not confirmed that it actually moves an *open position's* stop-loss when keyed by
the position's originating order id, nor what it does to the take-profit.

**This test is the gate before either feature is trusted on a live account.** It
does not block landing the code (the features are deploy-deferred); it blocks the
eventual live promotion.

### The exact code under test

`src/tradenation_adapter.rs`, `impl Broker for ... { async fn amend_stop(...) }`
(around line 270). Read its own comments — they spell out the two unknowns:

- **Lines ~282–285:** "UNVERIFIED: the upstream `amend_order` (`AmendCloseOrder`)
  has no callers and it is not yet confirmed it amends an OPEN position's SL
  keyed by the position's originating order id."
- **Lines ~300–305:** TradeNation's `AmendCloseOrder` requires **both** SL and
  TP on every amend. To move only the stop, the adapter re-sends the position's
  **existing TP unchanged**. If the position has **no TP**, it sends `0.0`, and
  it is "UNVERIFIED whether the platform reads that as 'no TP' or 'TP at 0'."

`amend_stop` re-fetches the account (`get_account_details`), locates the position
by `position_id` **or** `order_id` (`find_amend_target`), then calls
`tradenation_api::amend_order(client, session, order_id, market, stake, new_stop,
existing_tp)`.

---

## What "pass" looks like

After amending the stop of an open position to a new price, a fresh read-back of
that position from the broker must show:

1. ✅ **SL == the new stop price** you requested (within a tick).
2. ✅ **TP unchanged** — same value it had before the amend (NOT wiped, NOT moved,
   NOT set to 0).
3. ✅ The position is **still open** with the **same stake/direction** (the amend
   must not close, re-open, or resize it).

If all three hold, the path works and break-even / stop-widen can be trusted live.

## What "fail" looks like (and what each failure means)

- ❌ The amend call **errors** (`AmendError::*`) or the SL doesn't change on
  read-back → `AmendCloseOrder` does **not** accept the position's order id to
  move an open SL. The break-even cron would silently never protect a trade.
  **This is the most important thing to find out.**
- ❌ SL moves but the **TP gets wiped / changed / set to 0** → the "re-send
  existing TP" logic is wrong for this endpoint. Dangerous: a short with TP
  silently set to 0 could become a runaway. Must be fixed before live.
- ❌ The amend **closes or resizes** the position → wrong endpoint semantics
  entirely.

---

## The two test cases (run BOTH)

The TP behaviour is the riskier unknown, so test both the with-TP and the
no-TP shapes:

| Case | Open a position with… | Checks the… |
|---|---|---|
| **A — with TP** | a stop-loss **and** a take-profit | SL moves, **TP preserved** |
| **B — no TP** | a stop-loss, **no take-profit** | SL moves, no TP appears at 0 |

For both: pick a liquid instrument that's **open Monday** (e.g. **EUR/USD**,
market id `71402`, or **Spot Gold** `72318`). Use a tiny stake (demo min). Put
the SL comfortably away from price so it won't trigger during the test, and the
"new" SL also away from price but at a clearly different, checkable number.

---

## Route 1 (recommended) — MCP for state + a tiny Rust harness for the amend

The MCP `tradenation` server can **select the experimental account, open nothing,
but read positions** — it has no place-order or amend tool. So:

### Step 0 — confirm the account

```
# MCP tool: list_accounts   → confirm an "experimental" account exists.
# Select it for the session by exporting before MCP calls / harness runs:
export TN_ACCOUNT=experimental
```
(If there's no stored `experimental` account, `create_account {name:"experimental"}`
via MCP provisions a fresh demo, or use `get_browser_login_url {name:...}` to open
it in a browser.)

### Step 1 — open the test position

Two ways, pick one:

- **TradeNation web UI** (simplest): `get_browser_login_url {name:"experimental"}`
  via MCP, open it, manually place a small EUR/USD position **with a stop-loss**
  (Case A: also set a take-profit; Case B: leave TP empty).
- **Or** the `instruments buy` smoke-test path (see README "tradenation
  instruments buy = smoke test") — note whether it lets you attach SL/TP.

### Step 2 — read back the BEFORE state

```
# MCP tool: list_open_positions
# Record for the test position:
#   - position_id, order_id
#   - direction, stake, market
#   - current stop_loss, current take_profit
```

### Step 3 — amend the stop (the actual code under test)

Write a tiny Rust example that logs into the experimental demo and calls the
**worker's own** `amend_stop` so the exact production path runs. Sketch:

```rust
// e.g. broker-tradenation example, or a throwaway bin in this workspace.
// Deps: broker-tradenation (git dep already in Cargo.toml), tokio.
use trade_control_core::broker::Broker; // the trait carrying amend_stop

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    // Log into the EXPERIMENTAL demo. Confirmed constructors (cargo git dep
    // `tradenation-api`, tag broker-tradenation-v0.10.0):
    //   - broker_tradenation::login(session_json: &str) -> Option<TradeNationBroker>
    //       — the worker's path; needs a serialized Session blob.
    //   - tradenation_api::Session::login_demo_named("experimental") -> Result<Session>
    //       — logs into the named stored demo and returns the lower-level Session.
    //   - ::login_demo_with_creds(user, pass) / ::login_from_env() also exist.
    // Pick whichever lets you end up holding a `TradeNationBroker` (the type that
    // impls the `Broker` trait with amend_stop). If you only have a `Session`,
    // serialize it to JSON and pass to `broker_tradenation::login`, or look at
    // how the worker builds the broker from a cached session in
    // `src/tradenation_adapter.rs` / `src/cron/sweep.rs::acquire_broker_for_account`.
    let broker = /* construct TradeNationBroker for experimental */;

    // The id from Step 2. amend_stop accepts position_id OR order_id.
    let position_id = "<from step 2>";
    let new_stop = /* a clearly different, safe price */;

    broker.amend_stop("experimental", position_id, new_stop).await?;
    println!("amend_stop returned Ok");
    Ok(())
}
```

Notes for whoever writes the harness:
- `amend_stop`'s **first arg (`account_id`) is ignored** by the TN adapter — it
  re-fetches via the session. Pass anything.
- It accepts **either** `position_id` **or** `order_id` (`find_amend_target`
  matches both). Use the `position_id` from Step 2.
- If `amend_stop` returns `Err(AmendError::NotFound)`, the position id didn't
  match — re-check Step 2's ids.
- If it returns `Err(AmendError::Transient)`, the underlying `amend_order` call
  failed — capture the worker/adapter `rlog_err!` line; that error body is the
  diagnosis.

### Step 4 — read back the AFTER state

```
# MCP tool: list_open_positions   (again)
# Compare to Step 2:
#   - stop_loss == new_stop ?            (Step 2 success criterion 1)
#   - take_profit == before value ?      (criterion 2 — the risky one)
#   - direction/stake/position_id same ? (criterion 3)
```

### Step 5 — clean up

Close the test position (TN web UI, or MCP `get_browser_login_url` → close), so
the demo account is left flat.

---

## Route 2 — fully manual (no Rust)

If writing the harness is impractical, you can still partly verify the *broker
behaviour* (though not the adapter glue) by amending the SL **in the TradeNation
web UI** and reading it back via MCP `list_open_positions`. This confirms whether
the platform supports moving an open SL at all and what it does to TP — but it
does **not** exercise `find_amend_target` / the `existing_tp` re-send logic, so
it's a weaker test. Prefer Route 1.

---

## Report back

Produce a short result with, for **each** case (A with-TP, B no-TP):

```
Case A (with TP):
  before:  SL=<x>  TP=<y>  stake=<s>  dir=<d>  position_id=<p>
  amend:   new_stop=<n>   -> amend_stop returned: Ok | Err(<which>) <error body>
  after:   SL=<x'> TP=<y'> stake=<s'> dir=<d'>  open=<yes/no>
  verdict: PASS / FAIL (SL moved? TP preserved? still open same size?)

Case B (no TP):
  ...same shape...  (watch specifically for a TP appearing at 0)
```

Plus a one-line **overall verdict**:
- **PASS** → `amend_stop` on an open position works; break-even + stop-widen can
  be cleared for live. Update the "UNVERIFIED" comments in
  `src/tradenation_adapter.rs` and the demo-confirm caveats in
  `CHANGELOG.md` (v61) / `README.md` ("Break-even stop management") to "verified
  on experimental demo <date>".
- **FAIL** → capture the exact error / wrong field, and do **not** promote either
  feature to live. The break-even cron and the blackout widen both depend on this.

---

## Context the tester may want

- Break-even feature design + code map: see `TODO-breakeven-stop.md` in this repo
  and the `## v61` entry in `CHANGELOG.md`.
- The cron that will call this in production:
  `src/cron/breakeven_watch.rs` (every 15-min tick; joins open positions to
  their `EntryAttempt` break-even snapshot, then `amend_stop(entry)`).
- The sibling consumer with the same dependency:
  `src/cron/blackout_apply.rs` (`widen_one` → `amend`).
- This is **bug #2 of 3** from the demo-journal; it does not need the other two
  to be tested.
