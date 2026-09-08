# Stop entries silently drop on `#19-10` because `recover_entry` defaults to `skip`

**For:** the LLM working in `~/projects/trading-libraries/trade-control-web-hook`.
**From:** demo-journal trade 154 (USD/ZAR H1, plan `hs-usd-zar-1ac62120`), armed 2026-08-27.
**Verified against:** `main` @ `c7e95b4`. Line numbers are that commit — re-grep before editing.

**One-line:** the `recover_entry` machinery works and is shipped, but the *default* for a
plain stop entry is `Skip`, so a `#19-10` broker rejection silently forfeits the trade.
This is a **defaulting bug, not a missing feature.**

---

## Symptom

Live plan `hs-usd-zar-1ac62120` (USD/ZAR H1, short):

```
2026-08-27 20:11 ⊙ register → ok
2026-08-27 22:00 • fired 05-enter (enter) → entry-failed: too-close-to-market
2026-08-28 16:00 • fired 01-pause-… (pause)  → pause-set
2026-08-29 00:00 • fired 02-resume-… (resume) → pause-cleared
2026-08-29 01:00 • fired 01-veto-too-high (veto) → veto-set: … closed=failed
```

`05-enter` fired **once**, the broker rejected it `too-close-to-market`, and the plan then
sat inert until expiry. No recovery was attempted; no second entry was placed. The
replay of the same window fills and runs the trade to a stop-out.

The trade was **not** declined by any rule. It was dropped by a broker rejection that the
system already knows how to recover from.

---

## Mechanism (traced to code)

The recovery exists. `README.md:690-720` documents both `action: market` (chase, bounded
by `max_slippage_pips` or the derived SL→entry distance) and `action: limit` (rest at the
original trigger). Both re-run the full resolver tail — `min_r` floor, in-range check,
SL≥10×spread floor — so a recovered entry cannot bypass the gates. `CHANGELOG.md:5006`
(v57) renamed `on_too_close` → `recover_entry`; `core/src/recover_entry.rs` is the impl.

The problem is which default gets baked at arm time.

`tv-arm/src/hs_resolve.rs:409-418`:

```rust
recover_entry: match args.pattern_entry_mode() {
    Some(crate::args::PatternEntry::Limit) => args.limit_recover_action(),
    _ => args.recover_entry.map(|r| r.into_core()).unwrap_or(
        if args.require_confirmation {
            trade_control_core::intent::RecoverEntryAction::Limit
        } else {
            trade_control_core::intent::RecoverEntryAction::Skip   // <-- here
        },
    ),
},
```

So the baked default is:

| Arm shape | `recover_entry` default |
|---|---|
| `--entry-limit` | `Stop` (`args.rs:991` `limit_recover_action`) |
| stop entry **+** `--require-confirmation` | `Limit` |
| **stop entry, no `--require-confirmation`** | **`Skip`** ← the trade-154 case |

The BCR leg (`05-enter`) is a **stop at the geometry anchor** by default and this plan was
armed without `--require-confirmation` on that leg, so it landed in the `Skip` row.

Confirmed on the armed artifacts — `recover_entry` is **absent from the wire entirely**
(so the worker takes its own default) on both plans I checked:

```
usd-zar-h1-2026-08-27-…/plan.json   05-enter  entry.type=stop  recover_entry=<ABSENT>
gbp-chf-h1-2026-08-26-…/plan.json   05-enter  entry.type=stop  recover_entry=<ABSENT>
```

`mw_resolve.rs:331` is worse — it hard-codes `RecoverEntryAction::Skip` with no operator
override path at all.

---

## Why `Skip` is the wrong default for a stop entry

A stop entry is rejected `#19-10` **precisely when price has already run through the
trigger** — i.e. when the breakout the plan was waiting for is happening. `Skip` therefore
drops the trade in exactly the scenario the setup was designed to catch, and it does so
**silently**: no veto fires, no rule declines, nothing appears in the outcome ledger. The
plan just never enters.

This is the inverse of the `--require-confirmation` reasoning already encoded one line
above: that arm defaults to `Limit` because "the confirmation lag is exactly what strands
the stop." A fast breakout strands the stop the same way, without any confirmation lag.

---

## Journal frequency

Three occurrences in the demo journal, all silent forfeits:

| Trade | Instrument | Live result | As-designed replay |
|---|---|---|---|
| 049 | GBP/NZD | no entry (`too-close`) | trade ran |
| 050 | — | no entry (`too-close`) | trade ran |
| **154** | USD/ZAR H1 | no entry (`too-close`) | **−1.00R** |

Trade 154's replay lost, so the drop *happened* to save 1R — but that is luck, not
protection. The rule never ran. On 049/050 the same drop forfeited live trades. The
outcome distribution of dropped trades is currently unmeasurable, because a `Skip` leaves
no ledger row at all.

Note the sign convention this creates: **`Skip` biases the recorded ledger**, because
forfeited trades are invisible whether they would have won or lost.

---

## Suggested fix

Three parts, in priority order.

**1. Change the stop-entry default from `Skip` to a real recovery.** In
`hs_resolve.rs:409-418`, make an unconfirmed stop entry default to `Limit` (rest at the
original trigger — preserves planned R exactly, cannot fill worse) rather than `Skip`.
`Limit` is the conservative choice: it risks never filling, but it can never produce a
worse-than-planned fill, and the existing geometry guard already drops the degenerate
wrong-side case. `Market` is the aggressive alternative but needs a slippage bound to be
safe on a fast breakout; the derived SL→entry fallback exists for that, so `Market` is
defensible too — I'd want the two A/B'd on the fixture corpus before picking.

**2. Give `mw_resolve.rs:331` the same treatment.** It hard-codes `Skip` with no override;
at minimum it should honour `args.recover_entry`.

**3. Make the drop observable.** Whatever the default becomes, a `#19-10` that ends in no
entry should emit a distinct outcome token (the `recover-entry-<reason>` logging in
`README.md:717` exists for the *recovery* skip reasons — this needs to reach the plan
timeline and the outcome ledger, not just the log). Today a forfeited trade and a trade
that never triggered are indistinguishable downstream, which is what let this run three
times without being caught.

---

## Repro

```
replay-candles-staging --test-mode \
  --fixtures-glob 'usd-zar-h1-2026-08-27-*'
```

Fixtures on disk (all 8 entry-matrix variants):
`replay-fixtures/usd-zar-h1-2026-08-27-{normal,skip-bcr,strategy-v2,strategy-v2-qm-market}-news-{on,off}`

Replay fills and stops out (−1.00R on 6 of 8 cells); live drops the entry. The divergence
is the bug — replay has no broker, so it never sees `#19-10` and never exercises the
`Skip` path.

---

## Adjacent, not this bug

Worth logging separately, found while confirming the above:

- **`normal-*` cells return a bogus `0.00R`.** Both trendline anchors (epochs
  `1785506400` / `1785780000`, 2026-07-30 and 2026-08-02) fall **outside the fetched
  candle window**, so BCR/retest were index-estimated from `bar_seconds` and never
  stamped — 116 warnings, 4 unique. The fixture window needs widening before
  `normal = 0.00R` can be read as a real counterfactual. Not a `#19-10` issue.
- **`01-veto-too-high` logs `closed=failed`** when there is no position to close.
  `BUG-market-entry-no-broker-confirmation-trail.md` covers the close-failed conflation;
  this looks like the same conflation on the veto path.
