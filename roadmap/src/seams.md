# KV & broker seams (inventory)

A complete inventory of the two seams the recording/replay work must wrap: every
Cloudflare KV keyspace, and every broker-trait method + call site. Compiled from
source on 2026-06-14.

> Line numbers drift. Re-grep the symbol if one doesn't match.

## KV seam

**Binding:** `TRADE_CONTROL_KV` (`crate::KV_NAMESPACE`). **Wrapper:**
`KvStateStore` (`src/state/kv.rs`) implementing the `StateStore` trait
(`core/src/state.rs`). **Wrap the trait impl, not the call sites** — that's the
single choke point for `KvTransition` recording.

### Keyspaces (verbatim formats)

| keyspace | key format | value | scope | TTL |
|---|---|---|---|---|
| seen-id | `seen:{id}` | `"1"` | global | replay window |
| cooldown | `cooldown:{account\|None}:{instrument}` | `"1"` | acct/global | hours·3600 |
| prep | `prep:{account\|None}:{instrument}:{step}` | `{ts}\|{setter_id}` | acct/global | per-gate |
| veto | `veto:{account\|None}:{trade_id}:{instrument}:{name}` | `"1"` | acct/global | 6–48h |
| prep-blocked | `prep-blocked:{account\|None}:{instrument}:{step}` | `"1"` | acct/global | per-gate |
| entry-attempt | `entry_attempt:{scope}:{trade_id}:{n}` | JSON `EntryAttempt` | acct+trade | expires_at |
| retry-fire dedup | `seen-retry:{scope}:{trade_id}:{shell_time}` | `"1"` | acct+trade | replay window |
| pause | `pause:{trade_id}:{blackout_id}` | JSON `PauseEntry` | per-trade | 5–8h |
| news | `news:{trade_id}:{news_id}` | JSON `NewsEntry` | per-trade | 24–48h |
| blackout window | `spread-blackout:window` | JSON `SpreadBlackoutWindow` | singleton | backstop |
| blackout record | `spread-blackout:rec:{trade_id}` | JSON `SpreadBlackoutRecord` | per-trade | backstop |
| order body | `order:{broker_order_id}` | raw signed body | per-order | replay window |
| **indices** | `index:{seen\|cooldowns\|preps\|vetos\|prep-blocks}` | JSON array | global | none (advisory) |

> **Replay note:** the `index:*` lists are advisory but the `status` endpoint
> reads them. A KV snapshot for deterministic replay must include them.

### Key call sites

- **seen-id:** GET `lib.rs:149`; PUT `lib.rs:277`, `:1675`; DEL `lib.rs:2133`.
- **cooldown:** GET `lib.rs:1114`; PUT `lib.rs:474`; DEL `lib.rs:1697`.
- **prep:** GET `lib.rs:1147`; PUT `lib.rs:1793`; DEL `lib.rs:2119`, `core/state.rs:933`.
- **veto:** GET `lib.rs:1211`; PUT `lib.rs:757`, `:1939`, `:2052`; DEL `lib.rs:2172`.
- **prep-blocked:** GET `lib.rs:1753`; PUT `lib.rs:1866`.
- **entry-attempt:** LIST `retry_gate.rs:203`; PUT `retry_gate.rs:379`, `admin.rs:445`;
  update `retry_gate.rs:247,280`; LIST-all/DEL `cron/sweep.rs:35,189`.
- **retry-fire:** GET `retry_gate.rs:179`; PUT `retry_gate.rs:383`.
- **pause:** LIST `lib.rs:1048`; PUT `lib.rs:2221`; DEL `lib.rs:2252`.
- **news:** LIST `lib.rs:560`; PUT `lib.rs:2290`; DEL `lib.rs:2321`.
- **blackout window:** GET `lib.rs:1374`; PUT `cron/blackout_apply.rs:48`.
- **blackout record:** GET `cron/blackout_apply.rs:177`, `cron/blackout_cancel.rs:200`;
  PUT `cron/blackout_apply.rs:237`, `cron/blackout_cancel.rs:223`;
  LIST `cron/blackout_watch.rs:40`; DEL `cron/blackout_watch.rs:221`.
- **order body:** PUT `lib.rs:1510`; GET `cron/blackout_cancel.rs:124`; DEL `cron/blackout_restore.rs:201`.

> **Crash-safety ordering already present:** blackout widen/cancel write the KV
> record *before* the broker amend/cancel (`blackout_apply.rs` 237→319,
> `blackout_cancel.rs` 223→321). Recording must preserve this ordering so a
> replayed `KvTransition`-then-`BrokerCall` sequence matches reality.

## Broker seam

**Trait:** `trade_control_core::broker::Broker` (`core/src/broker.rs`).
**Implementors:** `TradeNationAdapter` (`src/tradenation_adapter.rs`),
`OandaBroker` (`broker-oanda/src/oanda.rs`). Wrap the trait for `BrokerCall` /
`BrokerResponse` recording.

### Methods + call sites

| method | returns | call sites |
|---|---|---|
| `place_entry` | `Result<String, EntryError>` (order id) | `lib.rs:1460`, `:1605` |
| `close_positions` | `bool` | `lib.rs:688`, `:2064` |
| `cancel_pending_for_instrument` | `usize` | `lib.rs:484`, `:2062` |
| `lookup_attempt_state` | `Result<AttemptState, LookupError>` | `retry_gate.rs:222`, `:237` |
| `cancel_order` | `Result<(), CancelError>` | `retry_gate.rs:231`, `cron/sweep.rs:166`, `cron/blackout_cancel.rs:321` |
| `get_quote` | `Result<Quote, LookupError>` | `lib.rs:1388`, + 4 cron sites |
| `list_open_positions` | `Result<Vec<OpenPosition>, _>` | `cron/blackout_apply.rs:306` |
| `amend_stop` | `Result<(), AmendError>` | `cron/blackout_watch.rs:160,168`, `cron/blackout_apply.rs:319` |
| `list_pending_orders` | `Result<Vec<PendingOrder>, _>` | `cron/blackout_cancel.rs:299` |
| `get_current_price` | `Result<f64, _>` (default over `get_quote`) | `lib.rs:604`, `:1563`, `cron/sweep.rs:120` |

### Broker-id observations

- `place_entry` returns the order id; it's stored into `EntryAttempt`
  immediately → `OrderPlaced` has its id at the seam.
- `broker_trade_id` (position id) is snapshotted by
  `set_entry_attempt_broker_trade_id` once a lookup finds an open position →
  `PositionOpened` has its id.
- **TradeNation closed-trade lookup returns `Cancelled` rather than a matched
  win/loss** because RefID ≠ PositionID — this is *exactly* why
  `PositionClosed` must be authoritative from our side, not reconstructed from the
  broker's id-less closed-trade row (see [event-schema](./event-schema.md)).
