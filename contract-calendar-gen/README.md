# contract-calendar-gen

Derives futures **contract close-out deadlines** and renders the committed
`core/src/contract_calendar_baked.rs` table that `core::contract_calendar`
reads.

## Why

IBKR **force-liquidates** an expiring futures position during a close-out
period preceding expiry, *without additional prior notice*, and does **not**
roll positions — "Automatic Futures Rollover" is a charting/data-line feature
that rolls nothing. Either we retire the position in time, or IBKR flattens it
at a price of its choosing.

The deadline is **not** the expiry date:

| | reference day | deadline |
|---|---|---|
| **Long** | First Notice Day | 2 business days before |
| **Short** | last trade day | 2 business days before |

For a physically delivered contract, First Notice Day is the last business day
of the month **preceding** delivery. So a **December** gold contract's long
deadline lands in **November** — about a month before the expiry date on the
contract chain.

Verified live on 2026-09-06: GCU6 (last trade 2026-09-28) was already past its
long close-out deadline while the chain still listed it as healthy front month.

## Run

```sh
# from the workspace root
cargo run -p contract-calendar-gen --release -- \
  --out core/src/contract_calendar_baked.rs
```

No network and no credentials — every date is derived from published exchange
rules, so the output is reproducible. `--dry-run` prints the report without
writing. Re-run when the holiday table gains a year or a contract root is added.

Unlike its sibling `market-hours-gen` (which measures candle gaps from live
broker feeds), this crate is pure arithmetic. IBKR's `ContractDetails` reports a
last-trade date but exposes **neither settlement type nor First Notice Day**, so
the chain cannot answer the question this table answers. The live chain is still
used as *validation*: the `rules` tests pin derived last-trade days against
readings taken from the paper Gateway.

## Layout

| file | holds |
|---|---|
| `holiday.rs` | CME/COMEX holidays + business-day arithmetic |
| `rules.rs` | contract specs and deadline derivation |
| `render.rs` | emits the baked table |
| `bin/generate.rs` | CLI driver + validation report |

## Contracts

| root | exchange | settlement | cycle | last trade | multiplier |
|---|---|---|---|---|---|
| GC | COMEX | physical | even months | 3rd-last business day | 100 |
| MGC | COMEX | physical | even months | 3rd-last business day | 10 |
| ES | CME | cash | quarterly | 3rd Friday | 50 |
| MES | CME | cash | quarterly | 3rd Friday | 5 |

Multipliers verified live against the paper Gateway 2026-09-06. They are
recorded for cross-checking only — the sizing path bakes its own value onto the
signed intent.

## Fail closed

Every fallible path returns `None`/`Err` rather than a default: unknown
contract, out-of-range year, ambiguous key, malformed row. The caller must treat
`None` as **refusal to arm**, never as "no constraint". A missing deadline that
reads as permission is exactly the failure that gets a position liquidated.

The holiday table covers **2026–2028**. Business-day arithmetic outside that
span would count weekends only and produce a deadline *later* than the truth —
the unsafe direction — so the generator refuses rather than extrapolating.
Extend `holiday::HOLIDAYS` to widen it.

## Known gaps

- **IBKR documents per-product close-out overrides** ("certain contracts use a
  different time ahead of the Close-Out deadline as specified in the following
  table"). That table is behind an anti-scraping block and has not been read, so
  it is unconfirmed whether GC/MGC/ES/MES carry an override on the standard 2
  business days. An override shorter than standard would make these deadlines
  too late. Verify before the first real-money futures position.
- **MGC settlement** is recorded as physical on the strength of CME's
  micro-metals literature (deliverable via an Accumulated Certificate of
  Exchange) and IBKR's own COMEX metals docs, which group MGC with GC. Several
  secondary sources claim cash-settled; see `rules.rs` for why they aren't
  followed. The error is asymmetric — physical is both the conservative and the
  better-sourced reading — but a final confirmation is still worth having.
- **ES/MES early liquidation**: no explicit IBKR sentence was found confirming
  cash-settled contracts simply run to final settlement. Believed true (the
  close-out apparatus is framed entirely around physically delivered
  contracts), and the table's cash-settled deadlines are conservative anyway.

## Mutation verification

Per the project's "green tests prove nothing" rule, each guard was broken and
the suite confirmed red.

| # | mutation | result |
|---|---|---|
| 1 | First Notice Day reads the delivery month, not the prior month | 5 tests fail |
| 2 | close-out offset 2 → 0 business days | 3 tests fail |
| 3 | Thanksgiving removed from the holiday table | 2 tests fail |
| 4 | duplicate keys silently deduped at render time | 1 test fails |
| 5 | long deadline counts from last trade day, ignoring FND | 4 tests fail |
| 6 | ES marked physically settled | 2 tests fail |
| 7 | ambiguous lookup takes the first row | 1 test fails |
| 8 | `is_past_close_out` answers "safe" for unknown contracts | 1 test fails |
| 9 | long/short deadlines collapsed into one | 3 tests fail |
| 10 | unrecognised settlement defaults to cash | 1 test fails |

Mutation 7 initially **survived** — the generator makes duplicates impossible,
so no test could reach the lookup's ambiguity guard. `lookup_in` was split out
to take an explicit table so the guard is exercised against a synthetic one.
An untested guard is one a later refactor deletes without a red test, which is
how `core/src/spread_blackout/coverage.rs` came to take the first match
silently.
