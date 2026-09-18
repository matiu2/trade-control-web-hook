# Re-bless 2026-09-18 (third) — duplicate promoted legs removed, expiry clock restored

Base: staging 3a07bca7. Branch `feat/replay-upkeep-fixtures`. 195 goldens moved
(all `expected.json`; no plan, candles or meta changed except the uk-100 message).
Measured cell by cell against a binary built from staging 2241c56b over the whole
corpus: 195 cells differ, 194 with FEWER legs, 0 with more, 1 with the same legs.

## 1. 194 goldens: a promoted park was a second position on the same trade

The 2026-09-18 re-blesses (3eced28f, 3a07bca7) booked "promoted park" legs as
truth. They were duplicates. A park sits ABOVE the retry gate, so nothing
deduplicated it, and a promotion skipped the gate and wrote no `EntryAttempt`
(`BUG-spread-park-bypasses-entry-dedup.md`). On H1 the shape is a fire that the
gate would have rejected (the trade already open — 326 such fires across the
corpus, every one `trade-already-open (backstop)`) parking below the SL floor and
being promoted on top of the filled first attempt.

Example `aud-cad-h1-2026-07-22-normal-news-off`: 2 legs +0.56R → 1 leg +0.26R —
the second leg was placed at 07:00Z while the first, filled at 06:00Z, was open.
`gbp-zar-h1-2026-07-27-*`: 4 legs → 3; the dropped leg re-placed the SAME broker
order and had been booked twice.

By legs (before → after): 2→1 ×96, 3→1 ×35, 4→2 ×16, 3→2 ×13, 4→1 ×10, 4→3 ×10,
5→3 ×10, 5→4 ×4. By setup: aud-cad-h1 52, aud-nzd-h1 40, gbp-cad-h1 26,
gbp-chf-h1 24, gbp-zar-h1 24, eur-zar-h1 12, eur-cad-h1 9, nzd-cad-h1 2,
nzd-usd-h1 2, usd-zar-h1 2, eur-aud-m15 1.

These cells are alternative configs of the same few trades, so do not read the
summed R: report per setup. The direction is a correction, not a strategy change —
live would have been holding two positions on one trade.

## 2. 1 golden: `uk-100-news-blackout-rentry-close-on-reversal`

The trade-expiry veto is a `TimeReached` trigger. `fire_rule`'s spread-hour
suppression classed it as a wick cross, and UK 100's mask covers 21:00–06:00
London, so the expiry at 02:00Z was held. The fixture window ends on that bar, so
the expiry close vanished: `final_phase await_entry`, position open at window end,
−1.00R. With the clock exempted the veto fires, the position is flattened at
expiry (+1.32R leg, net +0.32R) and the plan retires. This is the fixture's
ORIGINAL verdict shape; the tests `stateful_broker_books_expiry_closes_in_the_report`
and `all_fixtures_match_expected` are green again.

## Not covered

Fixture cells that exist only untracked in the primary checkout were not
re-blessed here; run `--test-mode --fixtures-glob '*' --rebless` there with a
binary built from this commit.
