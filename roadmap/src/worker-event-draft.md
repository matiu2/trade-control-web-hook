# Proposed `WorkerEvent` enum (draft)

A concrete Rust sketch for review **before** any code lands. **Not wired into
`core/`** — this is a design artifact. Fitted to the real types verified on
2026-06-14 (`Action`, `VetoLevel`, `EntryError`, `AttemptState`, the gate-outcome
enums).

> Decisions this encodes are in [event-schema](./event-schema.md); the field
> names follow the [Phase 0 notes](./phase0-implementation-notes.md) (`id` /
> `trade_id`, not invented names). Bikeshed freely — this is a starting point.

## The envelope (on every event)

```rust
/// Common header on every recorded event. Threaded so any consumer can
/// group by setup (`trade_id`), fire (`id`), or invocation (`request_id`),
/// and order within a fire (`seq`) or across fires (`ts`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EventEnvelope {
    /// UTC RFC3339 with offset. Cross-fire / cross-stage ordering.
    /// Never localized at emit — the operator's tools convert for display.
    pub ts: String,
    /// The intent's `id` — unique per fire. Always present.
    pub id: String,
    /// The intent's `trade_id` — the setup/position key. `None` on
    /// control actions (Prep/ClearPrep/Invalidate/Status/Unlock/PrepExpire).
    pub trade_id: Option<String>,
    /// Minted at the `fetch` entry point. NEW — does not exist today.
    pub request_id: String,
    /// Monotonic within one `request_id`. Intra-fire ordering, because
    /// multiple events share a `ts` to the second. NEW.
    pub seq: u32,
    /// Which fire this is for the `trade_id` (1 = first placement,
    /// 2.. = refires). Derivable from the `entry_attempt:` list.
    pub fire_seq: Option<u32>,
    /// Optional causal link to a prior event's (id, seq) — e.g. a
    /// BrokerCall points at the GateDecision that authorized it.
    pub parent: Option<(String, u32)>,
    /// `EUR_USD`-style if you pick OANDA-canonical, plus the broker form.
    pub instrument_canonical: Option<String>,
    pub instrument_broker: Option<String>,
    pub account: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorkerEvent {
    #[serde(flatten)]
    pub env: EventEnvelope,
    #[serde(flatten)]
    pub kind: EventKind,
}
```

## The variants (Phase 0 — debugging-first)

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
pub enum EventKind {
    /// The signed alert as received. Input to replay.
    AlertReceived {
        action: ActionTag,        // mirrors core::intent::Action
        raw_body: String,         // verbatim signed bytes
    },

    /// A gate's decision. The pure gates (allow_entry/allow_close/candle/
    /// too_close/spread_blackout) hand back an outcome value; the I/O gates
    /// (cooldown/veto/prep/retry) report the observed result.
    GateDecision {
        gate: GateTag,
        passed: bool,
        reason: Option<String>,   // reason code, e.g. "missing-prep"
        /// Veto gates only — lets a veto-driven close be traced (trade 046).
        veto_level: Option<VetoLevelTag>,
    },

    /// A KV read or write. Records FAILURES too — the highest-value debug
    /// record ("wanted to X, couldn't"). `before`/`after` enable replay
    /// snapshot-and-diff.
    KvTransition {
        key: String,
        before: Option<String>,
        after: Option<String>,
        op: KvOp,                 // Get | Put { ttl } | Delete
        success: bool,
        error: Option<String>,
    },

    /// A broker-trait call.
    BrokerCall {
        method: BrokerMethod,     // PlaceEntry | ClosePositions | CancelOrder | ...
        args: serde_json::Value,  // method-specific, structured
    },

    /// The broker's response. `error` carries the MOST SPECIFIC variant
    /// (EntryError etc.) — NOT a flattened/generic string. See the
    /// recordability audit's #19-10 lesson.
    BrokerResponse {
        order_id: Option<String>,
        position_id: Option<String>,
        fill_price: Option<f64>,
        error: Option<String>,    // e.g. "entry_too_close_to_market", not "order_rejected"
    },

    /// Order placed — carries the broker order id the moment it's known.
    OrderPlaced { broker_order_id: String },

    /// Authoritative close. Emitted EVEN for worker-uninitiated stop-outs,
    /// because TradeNation's closed-trade row is id-less.
    PositionClosed {
        broker_position_id: Option<String>,
        exit_type: ExitType,      // Tp | Sl | Manual | VetoClose | TradeExpiry | NeverFilled
        exit_price: Option<f64>,
        exit_time: Option<String>,
        // FX + realised P&L fields are LATER (reason-2). Omitted in Phase 0.
    },

    /// The dispatcher outcome, carrying both raw status + decoded variant.
    OutcomeRecorded {
        outcome: OutcomeTag,      // Ok | Failed | Rejected — mirrors ActionResult
        status_code: u16,         // 200 | 502 | 400 | 409 | 412 | 423 | 500 | 503
        detail: String,           // the outcome string, e.g. "entered"
    },

    /// The 409 "recognized a refire, deliberately did nothing". A distinct
    /// semantic event — NOT an error. (CHF/JPY incident.)
    ReplayGuarded { prior_outcome: Option<String> },

    /// Distinct from PayloadParseError — both are 400s today (a trap).
    IntentExpired { not_after: String },
    PayloadParseError { detail: String },
}
```

## Supporting tags (mirror existing enums; don't re-import to keep events WASM-light & stable)

```rust
// These deliberately MIRROR core types rather than re-export them, so the
// recorded JSON schema is stable even if the internal enums gain variants.
// Each has a `From<core::...>` conversion (not shown).

pub enum ActionTag { Enter, Close, Invalidate, Status, Unlock, Prep, Veto,
    PrepExpire, ClearPrep, ClearVeto, Pause, Resume, NewsStart, NewsEnd }

pub enum GateTag { AllowEntry, AllowClose, Candle, TooClose, SpreadBlackout,
    Cooldown, Veto, Prep, Retry }

pub enum VetoLevelTag { StopNextEntry, ClosePositions }

pub enum BrokerMethod { PlaceEntry, ClosePositions, CancelPendingForInstrument,
    LookupAttemptState, CancelOrder, GetQuote, ListOpenPositions, AmendStop,
    ListPendingOrders, GetCurrentPrice }

pub enum ExitType { Tp, Sl, Manual, VetoClose, TradeExpiry, NeverFilled }

pub enum OutcomeTag { Ok, Failed, Rejected }

pub enum KvOp { Get, Put { ttl_seconds: Option<u64> }, Delete }
```

## Design notes for the reviewer

- **`#[serde(flatten)]` envelope + `tag = "event_type"`** gives flat JSON
  objects (`{ "ts": ..., "id": ..., "event_type": "kv_transition", "key": ... }`)
  — greppable, one object per event in R2.
- **Tags mirror, not re-export.** If `core::intent::Action` gains a variant, the
  recorded schema doesn't silently change shape. The `From` impls are the single
  conversion point; a missing arm is a compile error (good — forces a conscious
  schema decision).
- **`args: serde_json::Value` on `BrokerCall`** keeps the envelope stable while
  allowing per-method payloads. Could be tightened to a typed enum later if
  replay-diffing wants stronger guarantees.
- **Phase 0 omits** the reason-2/3 fields (FX, realised P&L, MFE/MAE, geometry).
  `PositionClosed` is deliberately thin now; the FX/P&L fields slot in later
  without breaking existing records (additive).
- **`OrderFilled`/`PositionOpened`** are listed in the schema as Later — not in
  this Phase 0 enum. Add when the "did the stop trigger, when" gap is worth
  closing.

## Not-yet-decided (carried from open questions)

- Snapshot all touched KV vs. read-set only (affects whether `KvTransition`
  records every read or only writes + the read-set).
- Whether `BrokerCall.args` graduates to a typed enum.
- The canonical instrument form (OANDA `EUR_USD` vs. broker-agnostic `EURUSD`) —
  defer to `instrument-lookup`'s canonical id.
