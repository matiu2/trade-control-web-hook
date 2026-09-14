# EVIDENCE — does the spread trough last the whole baked hour, or recover mid-hour?

**Date:** 2026-09-14
**Question:** Inside a baked spread hour, does the elevated spread reliably
last the WHOLE hour, or does it commonly recover to a narrow spread partway
through? This decides whether `replay_broker.rs::get_quote`'s clamp (holding
the reported spread AT `elevated_threshold_pips` for the whole baked hour) is
a good approximation of live, or a source of replay↔live divergence.

**Bottom line — the answer is instrument-shape-dependent, not one number.**
There are (at least) three regimes in the data, and they behave completely
differently:

| regime | example | recovers <=4.0p before hour-end? | clamp accuracy |
|---|---|---|---|
| **thin/major FX tail-spike** | EUR/USD (OANDA) | **9 of 14 days (64%)** | clamp is WRONG most days — live's early release plausibly fires, replay's doesn't |
| **medium/thin FX cross, wide quiet spread** | GBP/AUD, AUD/CHF (OANDA) | **0 of 14 and 0 of 13 days (0%)** | clamp is RIGHT — live's early release structurally cannot fire (quiet spread already above the flat cutoff) |
| **structural, flat-wide-all-day** | XAU_USD / Gold (OANDA) | **not applicable — currently unflagged** (mask=0) | clamp never engages for this instrument at all |

Full detail, methodology, and numbers below.

---

## 1. Does an existing measurement exist?

**Partially yes, and it's better evidence than a fresh sample for the ONE
instrument it covers (GBP/AUD) — but it doesn't answer the general question,
because the shape varies by instrument (see §3).** I looked in this order:

- `SCOPING-candle-derived-spread-baseline.md` (repo root, 2026-07-13) — design
  doc for the current mask pipeline. Describes the **med3** rule
  (`ratio(h) = p90(spread/mid, h) / vol >= 3x median_over_hours(ratio)`)
  computed from **H1** bid/ask candles. This identifies *which hour* is
  elevated; it says nothing about within-hour dynamics. Superseded in its own
  particulars (see below — the shipped pipeline is minute-based, not H1-only).
- `TODO-spread-hours-per-instrument.md` (repo root, 2026-07-05) — an **older**
  analysis (sampler-era, hourly snapshots, TradeNation-only, since fully
  retired) that characterized some instruments (Gold, "many equities") as
  "elevated for a broad ~12h overnight block." **This characterization is
  STALE against the current pipeline** — see §3, the current baked Gold mask
  is empty (not flagged at all), and only 4 of 160 currently-baked rows have
  more than one elevated hour bit set. Don't rely on this doc's per-instrument
  shape claims; it predates the med3 + minute-based recalibration.
- **`spread-baseline-gen/src/compute.rs`** — the CURRENT, shipped generator.
  Its module docs and inline test names are calibrated against real minute
  data and are direct, load-bearing evidence:
  - `FLAG_PERCENTILE` doc (line 81-93): *"A real spike filling >=1/4 of the
    hour (OANDA EUR_USD 21:00 is short but genuine: p75 ratio 1.35x vs 1.24x
    threshold)"* — i.e. the generator's own calibration data already showed
    EUR/USD's spike does **not** fill the whole hour.
  - Test `minute_short_real_spike_flags_but_shorter_bleed_does_not` (line
    730-749): calibrated directly against the EUR_USD vs GBP_AUD minute
    ground-truth — encodes "a genuine spike that fills ~20 min of the hour
    (hour 21, EUR_USD-like short spike) MUST flag."
  - `spread-baseline-gen/examples/forecast_vs_reality.rs` — a committed,
    runnable audit tool built specifically to compare the generator's
    all-minutes population against an hour-closing-minute-only reconstruction,
    because these two populations diverge when a spike is short relative to
    the hour.
- **Memory `gbpaud_spread_hour_minute_truth`** (2026-07-14, in
  `~/.home-claude/.../memory/`) — a real, decisive 25-day minute-level probe
  (throwaway script `spread-baseline-gen/examples/gbpaud_zoom.rs`, **not
  committed**, so its raw output does not survive — only the memory's summary
  does). Findings for **GBP/AUD**, both brokers:
  - OANDA, 25 days: hour 20:00-20:54 UTC calm (~1.0x normal); spike onset
    **21:04 UTC** on 22/25 days; peak **~8.7x**; spike ends **~22:00 UTC**.
  - TradeNation, 1 day (API paging limit at the time): onset 21:05, peak
    8.3x, ends 22:00 — matches OANDA to the minute.
  - **Conclusion for GBP/AUD specifically: the spike essentially fills the
    WHOLE flagged hour, no mid-hour recovery.** This matches what I measured
    fresh below (0/14 days recovered).

**Methodology used to produce the shipped mask table** (verified against
current code, not the stale doc): `spread-baseline-gen` fetches OANDA M1 /
TradeNation M1 (paged) bid/ask candles, buckets every **minute's**
`spread_frac` by **schedule-local hour** (DST-invariant), flags an hour on its
**p75** minute-ratio vs the instrument's own volatility-relative median
(`profile_from_minutes` / `apply_gates`, `spread-baseline-gen/src/compute.rs`).
So the CURRENT bake already screens out short end-of-hour bleeds — but its
output is still a **binary elevated/not-elevated per hour**, not a
minute-by-minute trace. It cannot itself answer "in what fraction of hours
does the trough recover mid-hour" — that requires either the (unrecoverable)
raw minute samples from the bake run, or a fresh probe. I took the fresh-probe
path for everything except GBP/AUD, where the throwaway-script memory already
answers it.

**Pipeline provenance — confirmed CURRENT, not the old sampler.** `core/build.rs`
no longer exists (`ls core/build.rs` → not found). `core/src/spread_blackout.rs`
line 59 does `include!("spread_baseline_candle.rs")` directly — a static table
committed to git, last regenerated **2026-08-03** (`git log -1` on that file:
commit `1cd24ae6`, *"data(spread): re-bake all 125 OANDA rows; census the mask
against the NY-close rule"*). `docs/FOLLOWUP-retire-spread-sampler.md` confirms
the old H1-close/TradeNation-only sampler's code consumption was deleted in
v91 (2026-07-19, commit `78bd280`, net -201 lines: `core/build.rs`, the
`mod baseline` include, `baked_spread_hours`, `spread_hour_widen_pips`, the
`[build-dependencies]` section). **There is one spread pipeline today** (the
candle/minute-derived generator); the shipped mask is not sampler-derived.

## 2. The actual constants (verified against code, not assumed)

- `SPREAD_BLACKOUT_ELEVATED_PIPS = 8.0` — flat, instrument-agnostic *fallback*
  reject threshold (`core/src/spread_blackout.rs:505`), used only when an
  instrument has no baked baseline.
- `SPREAD_BLACKOUT_RECOVERED_PIPS = 4.0` — flat, instrument-agnostic recovery
  cutoff (`core/src/spread_blackout.rs:518`). **This is the number that
  actually gates live's early release** — `spread_recovered()` in
  `core/src/pending_lifecycle.rs:1276-1278` is `spread_pips <=
  SPREAD_BLACKOUT_RECOVERED_PIPS`, called from `pending_lifecycle.rs:969` with
  no per-instrument variant. Test `spread_recovered_below_and_at_cutoff`
  (line 1332-1337) pins `4.0` recovered, `6.0` NOT recovered ("hysteresis band
  is not recovered"), `20.0` not recovered.
- `elevated_threshold_pips(instrument) = median_pips * SPREAD_REJECT_MULTIPLE`
  where `SPREAD_REJECT_MULTIPLE = 5.0` (`spread_blackout.rs:277,290-303`) —
  **per-instrument**, read from the baked `SPREAD_BASELINE_CANDLE` table. This
  is a DIFFERENT number from `SPREAD_BLACKOUT_RECOVERED_PIPS` and governs (a)
  the System-1 entry-reject gate, and (b) the value the replay clamp in
  `get_quote` holds the spread AT while inside a spread hour
  (`half = elevated_threshold_pips(instrument) * pip_size / 2.0`,
  `replay_broker.rs` ~line 1300). It is NOT the number `spread_recovered`
  compares against — **the flat 4.0p is what decides live's early release**,
  confirmed by reading both call sites.
- Hysteresis invariant `RECOVERED (4.0) < ELEVATED (8.0)` is asserted directly
  in a test (`spread_blackout.rs:767`).

From the current baked table (`core/src/spread_baseline_candle.rs`, whole-window
p10/p50/p90 in pips, columns 7-9 of each row):

| broker | symbol | median_pips (whole window) | low (p10) | high (p90) | elevated_threshold_pips (median*5) |
|---|---|---|---|---|---|
| oanda | EUR_USD | 1.6 | 1.5 | 1.7 | 8.0 |
| oanda | GBP_AUD | 5.2 | 4.6 | 6.1 | 26.0 |
| oanda | AUD_CHF | 1.5 | 1.4 | 1.8 | 7.5 |
| oanda | XAU_USD | 63.0 | 45.0 | 84.0 | n/a (mask empty) |

**Important secondary finding**: GBP/AUD's own whole-window p10 (its quietest
observed spread, 4.6p) is already ABOVE the flat `SPREAD_BLACKOUT_RECOVERED_PIPS`
cutoff (4.0p). Across the full baked table (98 reviewed, masked instruments),
**30 of 98 (~31%)** have a whole-window p10 already above 4.0p — for these,
live's early-recovery release is close to structurally unreachable *regardless
of trough duration*, because the instrument's spread essentially never gets
that tight even outside the spread hour. This is a separate, additional reason
the clamp is harmless for those instruments, independent of §3's timing
finding.

## 3. Fresh minute-by-minute measurement (OANDA, live API, read-only)

Existing evidence answers GBP/AUD decisively but nothing else. Per the task's
instructions, I took a small, well-chosen fresh sample rather than broad
coverage: **EUR/USD, GBP/AUD, AUD/CHF** (all OANDA, all currently baked with
mask bit 17 = single-hour 21:00 UTC block, "ny" schedule) plus a spot-check
of **XAU_USD / Gold**.

**Method.** A standalone, read-only Rust binary (`tmp-spread-probe`, built as
a sibling crate outside this repo — no source file in this repo was modified)
used `oanda-client::OandaClient::get_candles`/`get_candles_to` to pull ~20,000
M1 (`price=MBA`) candles per instrument (~14 days), computed
`spread_pips = (ask_close - bid_close) / pip_size` per minute, and grouped by
UTC hour 20/21/22 (hour 21 is the flagged hour; 20/22 are context).

**UTC-window sanity check (rigour requirement).** All sampled dates are
2026-08-25 to 2026-09-13 — entirely within US EDT (spring-forward was
2026-03-08, fall-back is 2026-11-01), so per the empirically-confirmed DST
model (`spread_hour_tracks_us_dst_confirmed` memory, "ny" schedule FX pairs
spike at 5pm NY = 21:00 UTC in EDT), UTC hour 21 is the correct window to
sample and matches the baked mask bit (17) for the "ny" schedule. Verified,
not assumed.

**Recovery test applied**: for each sampled occurrence of the baked hour, find
the LAST minute where `spread_pips > SPREAD_BLACKOUT_RECOVERED_PIPS (4.0)`. If
that minute is before the hour's last minute (:59), the spread recovered and
STAYED recovered for the rest of the hour — this is exactly the condition that
would let live's `spread_recovered()` release a resting-order hold before the
baked hour ends, diverging from replay's clamp.

### EUR/USD (OANDA) — 14 sampled occurrences of the 21:00 UTC hour

**9 of 14 days (64%) recovered to <=4.0p before the hour ended and stayed
there. 2 more days never exceeded 4.0p at all. Only 3 of 14 (21%) stayed
elevated the whole hour.**

```
2026-08-25: did NOT recover — spread >4.0p persists to hour end (max=6.80p)
2026-08-26: RECOVERED at minute :48, stayed <=4.0p rest of hour (max=5.60p)
2026-08-27: RECOVERED at minute :18, stayed <=4.0p rest of hour (max=7.70p)
2026-08-30: RECOVERED at minute :59, stayed <=4.0p rest of hour (max=5.90p)
2026-08-31: RECOVERED at minute :11, stayed <=4.0p rest of hour (max=4.30p)
2026-09-01: RECOVERED at minute :33, stayed <=4.0p rest of hour (max=7.10p)
2026-09-02: never exceeded 4.0p this hour at all (max=4.00p)
2026-09-03: did NOT recover — spread >4.0p persists to hour end (max=10.00p)
2026-09-06: RECOVERED at minute :59, stayed <=4.0p rest of hour (max=4.50p)
2026-09-07: never exceeded 4.0p this hour at all (max=2.70p)
2026-09-08: RECOVERED at minute :54, stayed <=4.0p rest of hour (max=5.80p)
2026-09-09: RECOVERED at minute :21, stayed <=4.0p rest of hour (max=10.00p)
2026-09-10: RECOVERED at minute :54, stayed <=4.0p rest of hour (max=10.00p)
2026-09-13: did NOT recover — spread >4.0p persists to hour end (max=5.60p)
```

Two representative raw traces (minute-by-minute, pips):

**2026-09-09 — clean fast recovery:**
`21:04=10.0, 21:05=9.7, 21:08=9.5, 21:09=6.2, 21:10=4.3, 21:11=3.9(recovered),
21:14=5.2(re-spike), 21:16=4.3, 21:20=4.0, 21:21=3.5 ... settles 2.3-3.2p for
21:30-21:59` — the trough recovers by ~:11-:20 and, apart from one blip back to
5.2p at :14-15, stays under 4p for the back half of the hour.

**2026-09-03 — noisy, borderline, classified "did not recover":**
`21:04=10.0 ... decays through the 6-9p band to 21:32-21:34=4.0p (right at the
cutoff) ... 21:35-21:36=3.6p (briefly recovered) ... 21:37-21:53 oscillates
4.0-5.2p (repeatedly crossing back above 4.0) ... 21:56-21:57=4.0/3.6p ...
21:59=4.6p`. This day shows the trough genuinely straddling the hysteresis
line rather than cleanly staying elevated — a live worker sampling at an
arbitrary instant in the back half of this hour would see sub-4p roughly half
the time.

**Interpretation**: EUR/USD's spike is short and front-loaded (onset ~21:04,
consistent with the `gbpaud_spread_hour_minute_truth` / compute.rs calibration
that EUR_USD's spike is "short but genuine" and fills roughly a quarter to a
third of the hour), and the back half of the hour is frequently back under the
flat 4.0p recovery cutoff. **For EUR/USD, replay's clamp is a poor
approximation on the majority of sampled days** — live's `spread_recovered()`
plausibly fires and releases a held resting order before the baked hour ends,
which replay cannot reproduce.

### GBP/AUD (OANDA) — 14 sampled occurrences of the 21:00 UTC hour

**0 of 14 days (0%) recovered.** Spread stays far above 4.0p for the entire
flagged hour every single day (min-within-hour ranged 15.6-25.8p, well above
the flat cutoff; max ranged 28.2-45.0p — note 45.0p recurs, suggesting a
broker-side spread cap/ceiling on some ticks).

```
2026-08-25: min=18.40p max=45.00p
2026-08-26: min=18.40p max=45.00p
2026-08-27: min=18.10p max=37.10p
2026-08-30: min=18.40p max=36.50p
2026-08-31: min=16.80p max=28.20p
2026-09-01: min=18.10p max=35.00p
2026-09-02: min=15.60p max=32.50p
2026-09-03: min=18.40p max=40.70p
2026-09-06: min=25.80p max=45.00p
2026-09-07: min=18.40p max=45.00p
2026-09-08: min=18.40p max=39.80p
2026-09-09: min=17.50p max=45.00p
2026-09-10: min=24.80p max=45.00p
2026-09-13: min=18.40p max=45.00p
```

This matches the existing `gbpaud_spread_hour_minute_truth` evidence exactly
(spike essentially fills the whole hour, ends ~22:00) and extends it with 14
fresh days confirming zero exceptions. **Secondary note**: GBP/AUD does
frequently dip below its OWN per-instrument `elevated_threshold_pips` (26.0p)
within the hour — between 1/45 and 51/56 sampled minutes per day were <=26p —
but that number governs the System-1 entry-reject gate and the replay clamp's
held VALUE, not the `spread_recovered()` early-release check, which uses the
flat 4.0p and never sees it here.

### AUD/CHF (OANDA) — 13 sampled occurrences of the 21:00 UTC hour

**0 of 13 days (0%) recovered.** Min-within-hour 11.9-15.0p, max 11.9-15.0p —
this instrument shows a suspiciously flat ceiling at 15.0p across nearly every
day (again suggestive of a broker-side spread cap on this cross during the
window, not organic tick noise).

```
2026-08-25: max=15.00p   2026-08-26: max=15.00p   2026-08-27: max=15.00p
2026-08-30: max=15.00p   2026-08-31: max=13.30p   2026-09-01: max=13.30p
2026-09-02: max=15.00p   2026-09-03: max=15.00p   2026-09-07: max=15.00p
2026-09-08: max=15.00p   2026-09-09: max=15.00p   2026-09-10: max=11.90p
2026-09-13: max=15.00p
```

### XAU_USD / Gold (OANDA) — spot-check, no currently-baked spread hour

**The old TODO doc's "Gold is elevated for a broad ~12h overnight block"
characterization is STALE against the current pipeline.** The current baked
row is `("oanda", "XAU_USD", "ny", true, 0, ...)` — **mask = 0, reviewed =
true**. This means the current med3+peak-frac generator looked at Gold and
found **no hour clears the 3x-relative-to-median bar** — consistent with the
code comment at `compute.rs:61-63`: *"on a genuinely FLAT instrument (Spot
Gold, peak only ~1.25x its median) nothing clears 3x-median."*

I confirmed this directly with a fresh full-day hourly profile (Sept 2026, 13
days, OANDA M1):

```
hour   n    p50    p90    max
   0  240  55.00  70.00   89.00
   1  240  57.00  76.00  103.00
   6  240  52.00  68.00   92.00
  12  180  57.00  78.00  208.00
  17  216  53.00  69.00   86.00
  20  240  56.00  88.00  246.00
  21    -      -      -       -   (no candles in this window that day range)
  22  224  76.00  99.00  138.00
  23  240  59.00  77.00   93.00
```

(Full 24-hour table available in the scratch output; median spread ranges
only 48-59p across every single UTC hour of the day — no meaningful daily
cycle, confirming the flat-wide characterization, just not a specific "12h
block.") **Gold is currently not gated by the spread-hour mechanism at all**
— `is_spread_hour("XAU_USD", ...)` returns false for every hour (mask 0, and
the fallback `ny_clock::is_ny_close_edge` would only apply if `reviewed` were
false — it's `true`, so the empty mask is authoritative, not a fallback gap).
So the replay-clamp-vs-live-recovery question is **moot for Gold under the
current bake** — neither system ever enters a Gold spread-hour hold to begin
with. This is worth flagging on its own: if the earlier "Gold overnight ≈
70-80p" widen intuition (`TODO-spread-hours-per-instrument.md`) was relied on
anywhere downstream (e.g. System-2 stop-widen sizing), it is currently dead —
Gold gets no baked widen either, for the same reason (empty mask).

## 4. Hysteresis band — read from code, not assumed

- **Elevated** (flat fallback only): `SPREAD_BLACKOUT_ELEVATED_PIPS = 8.0`
  (`core/src/spread_blackout.rs:505`).
- **Recovered**: `SPREAD_BLACKOUT_RECOVERED_PIPS = 4.0`
  (`core/src/spread_blackout.rs:518`), and this is the number that actually
  gates `spread_recovered()` / live's early release — confirmed at the call
  site (`core/src/pending_lifecycle.rs:969,1276-1278`).
- These two are NOT symmetric with the per-instrument `elevated_threshold_pips`
  (median*5) that the replay clamp and the System-1 reject gate use. Three
  different numbers are in play depending which mechanism you're looking at;
  conflating them was a risk in this investigation and I kept them separate
  throughout (see the table in §2).

## 5. Instrument-to-instrument variation — the headline finding

The trough-duration question does **not** have one answer. Three clearly
different regimes showed up in real data:

1. **Short, front-loaded tail-spike (EUR/USD)**: the spike is concentrated in
   roughly the first third of the hour; the back half frequently (9/14, 64%
   measured) settles back under the flat 4.0p recovery cutoff and stays there.
   **This is the regime where the replay clamp is a real, measurable
   approximation error** — replay holds the spread at ~8p (its
   `elevated_threshold_pips`) for the full hour while live would very plausibly
   see the spread recover and release a held order 20-50 minutes early on a
   majority of days.
2. **Sustained-wide-all-hour cross (GBP/AUD, AUD/CHF)**: spread stays multiples
   of the flat cutoff for the entire hour, zero exceptions in 27 combined
   sampled days. **The clamp is correct here** — there is nothing for live to
   recover into; both mechanisms would agree the hold should last the whole
   hour. Compounding reason: these instruments' *quiet* spread already sits
   at or above the 4.0p cutoff (GBP/AUD p10=4.6p), so early release is close
   to structurally unreachable independent of timing.
3. **Structural flat-wide-all-day (Gold)**: not currently gated by the
   spread-hour mechanism at all (empty mask) — the question doesn't apply.

Given the currently-baked table is dominated by single-bit (single-hour) masks
(94 of 160 rows) with only 4 multi-bit rows and 62 empty, and given EUR/USD's
regime (short spike, majority-recovers) versus GBP/AUD's regime (sustained,
never-recovers) sit on the SAME single-hour mask shape yet behave oppositely,
**the mask's bit-width (single-hour vs multi-hour) is not a reliable predictor
of trough duration** — the instrument's own spike-width-within-the-hour
matters and has to be checked per instrument, not inferred from the mask
shape.

## 6. Confidence level and what remains uncertain

- **High confidence** on the three measured instruments' individual behaviour
  (14, 14, and 13 real days each, freshly pulled from OANDA, cross-checked
  against the pre-existing GBP/AUD probe which agrees exactly).
- **High confidence** that the answer is instrument-dependent and that a
  single aggregate percentage across all instruments would be misleading —
  averaging EUR/USD's 64% against GBP/AUD's 0% produces a meaningless number.
- **Low confidence generalizing beyond these three (+Gold) instruments.** I
  did not sample the other ~90 single-bit-masked instruments in the current
  table (majors/crosses/other), nor any TradeNation-side instrument (no
  TradeNation client was wired into the fresh probe — time-boxed to OANDA).
  The existing GBP/AUD probe DID confirm TN and OANDA agree to the minute for
  that one pair, which is modest evidence (not proof) that OANDA-only sampling
  is representative of TradeNation's shape too, but this has not been checked
  broadly.
- **The 45.0p / 15.0p recurring maxima on GBP/AUD and AUD/CHF** look like a
  broker-side spread ceiling rather than organic price action (worth a
  separate look if anyone relies on the exact magnitude of these instruments'
  peak spread, but irrelevant to the recovery-timing question this file
  answers).
- **I did not re-derive or validate the med3/peak-frac mask-generation
  algorithm itself** — only consumed its documented calibration and the
  freshly-measured minute data. That algorithm's correctness is out of scope
  for this question and was not re-audited here.

## Practical read for the replay-vs-live parity decision

The clamp's own code comment already reasons about this qualitatively
("dips narrow on some bars... unreliable recovery signal... ping-pong"). This
audit adds quantitative grounding:

- **For sustained-wide instruments (GBP/AUD-shape), the clamp is not an
  approximation — it's the right answer**, confirmed by 0/14 and 0/13 real
  days with zero exceptions.
- **For short-tail-spike instruments (EUR/USD-shape), the clamp is a real,
  frequent (measured 64%) source of replay-holding-longer-than-live**, i.e.
  replay would report a resting order still held/cancelled during minutes
  where live had already released and possibly re-filled it. Whether this
  matters in practice depends on how much P&L moves in the released-but-not-
  replayed window — not measured here (out of scope: this file answers
  "does the trough recover," not "does the recovery change trade outcomes").

---

# ADDENDUM (2026-09-14) — evaluating the proposed fix: replace the clamp with `hour_p90_frac`

**Verdict: DO NOT SHIP the proposed change as specified.** It does not fix the
motivating case, and it carries a large unintended side effect. No code change
was made. Detail below; the analysis is reproducible from the baked table alone.

## A. The proposal

Replace `replay_broker.rs::get_quote`'s in-spread-hour clamp
(`elevated_threshold_pips = baseline_median_pips × 5`) with the baked per-hour
forecast `hour_p90_frac` (column 10), converted via `widen_frac_to_pips`.

## B. Method — converting column 10 to pips without a price feed

`hour_p90_frac` is a `spread/mid` fraction; `baseline_median_pips` is pips. The
ratio is scale-free:

```
forecast_pips(h) = baseline_median_pips × hour_p90_frac[h] / median(hour_p90_frac[nonzero])
```

Validated independently: feeding the *normal-hour* frac through
`widen_frac_to_pips` at realistic mids reproduces the row's own
`baseline_median_pips` (EUR/USD 0.0001487 × 1.05–1.25 / 0.0001 = 1.56–1.86p vs
baked 1.6p; GBP/AUD 5.31–5.89p vs baked 5.2p). GBP/AUD's resulting forecast
(39–44p) also lands inside §3's independently measured 28–45p range.

## C. Finding 1 — the fix does NOT fix EUR/USD, the motivating case

| instrument | clamp today | forecast (proposed) | reads recovered (≤4.0p)? | measured reality |
|---|---|---|---|---|
| EUR_USD | 8.00p | **6.85p** | **No** (both) | 9/14 days (64%) recover |
| GBP_AUD | 26.00p | 39.13p | No (both) | 0/14 recover |
| AUD_CHF | 7.50p | 13.96p | No (both) | 0/13 recover |

For EUR/USD to read recovered, `frac × mid / pip ≤ 4.0` requires `mid ≤ 0.629` —
a price EUR/USD has never traded at. **The forecast is above the 4.0p recovery
cutoff for EUR/USD at every realistic price**, so replay still can never read as
recovered. The stated goal (let replay reproduce live's early release on the
64%-of-days instrument) is not achieved.

The reason is structural: `hour_p90_frac` is, by construction, the **p90 of
within-hour minute spreads** — `compute.rs` calls it "the spike magnitude" and
uses it to size a protective stop widen. A 90th percentile is a *peak*
statistic. The thing that makes live release early is the *typical late-hour
minute* (~2–4p on EUR/USD), which is nearer the p25 of the hour. Swapping one
peak-ish constant (median×5 = 8.0p) for another (p90 = 6.85p) moves the number
14% and changes no decision.

## D. Finding 2 — a large unintended side effect on the ENTRY gate

`get_quote` feeds two gates, not one. The second is the spread-blackout entry
gate (`dispatch/enter.rs:562`), and `spread_blackout_decision` is **strictly
`>`**: `window_open && spread_pips > threshold_pips`.

The clamp reports *exactly* `elevated_threshold_pips`, so `8.0 > 8.0` is false —
**the clamp sits precisely on the permissive boundary and the entry gate never
rejects under it.** (The clamp's own comment claims "the entry gate correctly
sees the trough"; that is not what the code does.) The replay does open the
window — `replay.rs:358` seeds it from `last_ny_close_edge_in_span` — so this
path is live in the corpus.

Because the forecast (p90) generally exceeds median×5, swapping it in flips that
boundary:

- **99 of 109 masked instrument-hours (91%) would change from ALLOW to REJECT.**

That is a corpus-wide entry-suppression change riding in on what was scoped as a
spread-reporting fix. It may even be closer to live for genuinely-wide bars, but
it is a separate decision with its own evidence bar, and it must not be shipped
as a side effect.

## E. Finding 3 — the anti-ping-pong premise is already false today

The brief assumed the clamp's safety came from always exceeding the 4.0p
recovery threshold. Measured against the baked table, that is not true now:

- **9 of 109** masked instrument-hours have a **clamp already ≤ 4.0p** (TradeNation
  `EUR/USD` 2.50p, `AUD/USD` 2.00p, `USD/JPY` 3.00p, `USD/CAD` / `USD/CHF` /
  `EUR/JPY` 3.50p, `EUR/GBP` 2.50p, …) — these already read as "recovered" every
  in-block bar.
- Under the forecast that drops to **2 of 109** (`SG30_SGD` 1.49p, `HKD_JPY` 1.79p).

So on the narrow anti-ping-pong axis the forecast is *better* than the clamp
(2 at-risk rows vs 9). Worth recording, but it does not rescue the proposal —
neither value oscillates in the first place, because both are **constant within
the hour**, which is the real anti-ping-pong property. Ping-pong was never a
risk for any constant; it was a risk for the raw per-bar close-spread.

## F. Finding 4 — the SL-vs-spread floor is NOT fed by the clamp

The brief flagged a possible sizing consequence. Checked: it does not arise.
`run_enter`'s floor prefers `windowed_entry_spread`, which reads
`get_bidask_candles` — served in replay from the **real recorded bid/ask series**,
unclamped (`replay_broker.rs:1473`). `get_quote` is only the *fallback* when the
window is unavailable, which requires `enter_granularity == None`; the replay
always passes `Some(granularity)` (`replay.rs:1076`). **No corpus entry sizes its
stop off the clamped 8.0p.** The clamp's blast radius is the entry gate and the
hold-release check only.

## G. What would actually fix EUR/USD

The gap is a *within-hour time profile*, which no current column carries — the
bake stores one number per hour, and the real signal is "the spike occupies the
first ~third of the hour". Options, roughly by cost:

1. **Accept the divergence, document it.** Replay holds for the full baked hour;
   live may release early on short-spike instruments. Cheapest, honest, and
   matches what the code comment already half-says.
2. **Bake a within-hour decay/duration column** (e.g. p50 alongside p90, or a
   spike-fraction) and have replay report the decayed value after the spike
   window. Needs a `spread-baseline-gen` regenerate and a table migration.
3. **Report the real bar close-spread but hold the OFF-side on the baked clock.**
   Decouples the two consumers: the entry gate sees the real bar, while
   `spread_hour_released` keeps its deterministic baked-hour-end signal, so no
   ping-pong is possible by construction. This is probably the right shape if
   the goal is fidelity, but it is a different change from the one scoped.

Recommend **(1) now**, with **(3)** as the designed follow-up if replay↔live
parity inside spread hours turns out to move P&L.

## H. Confidence and residual uncertainty

- **High** on the arithmetic (derivation independently cross-validated twice) and
  on the code-path reads (entry gate `>`, SL floor windowed, replay seeds the
  window) — all read directly from source.
- **High** that the proposal misses EUR/USD; that follows from an inequality with
  large margin, not a borderline estimate.
- **Medium** on the exact 99/109 count: it uses the scale-free estimator rather
  than each instrument's live mid. Rows near the boundary (`AUD_USD` 6.5 vs 7.45)
  could flip either way; rows far from it (the large majority) cannot. The
  headline — "the great majority flip to reject" — is robust.
- **Not measured:** whether the entry-gate flip would help or hurt P&L. That is
  the evidence a decision on option (3) would need.
