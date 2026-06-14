# Trade reconstruction & journaling

**Status:** not started · **Phase:** 5 · **Size:** medium (mostly a consequence)

Reconstructing and journaling a trade currently takes **1–3 hours** of manual
archaeology (see `trading-tax-tracker/analysis-data` and the demo journal).
Recording (Phase 0) shrinks the *reconstruction* half; the *journaling* half
stays human + LLM for the foreseeable future.

## Not a near-term goal: full automation

Full auto-journaling is **not** expected any time soon — it'll keep needing an
LLM in the loop. The realistic near-term win is **timeline recreation** (which
falls out of recording), grown slowly. This page is mostly reason-2/3 future
work behind the [debugging-first schema](./event-schema.md).

The operator already annotates intent onto the chart at arm time and reads it
back when journaling. The **chart is the journal substrate**; full pattern
geometry (7 H&S / 4 M/W anchors) is drawn *after* the trade, by hand — it lives
on the chart and in the overlay, never in the event stream.

## It's mostly downstream of recording

If the worker records every broker call + response + KV transition — all
threaded by the [correlation keys](./event-schema.md#correlation-keys-on-every-event)
— then "reconstruct the timeline" becomes:

```
filter events by correlation_id, sort by (ts, seq)
```

The hours of manual *joining* largely vanish; the narrative/judgement layer
stays human.

## What to get right now (so we don't regret it)

A **stable, structured event schema with a correlation id on every record** —
this is exactly [Phase 0](./event-schema.md). The choice (decided there): typed
events, **not** a code-lookup dictionary, because a dictionary rots and lives
apart from the data.

This directly kills today's documented pain:

- "CF-log joins are composite/fuzzy"
- "TN closed trades have no position_id"

A correlation id stamped on every event at write time means future joins are
exact, not reconstructed.

## The unit is the position, not the fire

Confirmed by the journaling LLM from real cases: one *setup* spawns several
fires (degraded refill = 2 intents/1 position; stop-then-re-entry = 2 rows/1
conceptual trade; bug-dependent fill/no-fill = same position, two narratives).
So the journal-entry key is **`correlation_id`** (the position/setup), with
`intent_id` per fire underneath. See [Event schema](./event-schema.md#decided-the-three-level-correlation-key).

## Render = events + overlay

Two stores, merged at render time:

- **Event stream** (immutable machine record) → every `[R]` raw fact and every
  `[D]` derived field (geometry, R:R, hold time, % to TP).
- **`journal_overlay`** (operator-authored, editable) → `notes`, `lessons`,
  `screenshot_url`, `manual_field_overrides`, `exclude_from_stats`. Keyed by
  `correlation_id`. Kept *separate* so re-rendering never clobbers human edits,
  and a bug-artifact trade can be a page but excluded from edge stats.

## What's still real work

- A **renderer**: `render(events) + merge(overlay)` → the spreadsheet row(s) and
  the mdbook page (existing demo-journal format).
- **Late events** are the journal's spine, not an afterthought: `OrderFilled`,
  `PositionClosed` (with `exit_type` + reason + native/home P&L + dated FX
  rate), MFE/MAE. The journaling LLM must **not** poll the broker — by journal
  time the position is gone and the broker candle API is unreliable. The schema
  delivers these as events.
- Back-filling: old trades predate the schema and still need the manual path.

## Acceptance

- [ ] `trade-control timeline <correlation_id>` emits an ordered event timeline.
- [ ] A renderer produces a draft journal entry (spreadsheet row + mdbook page)
      from `events + overlay`.
- [ ] `journal_overlay` edits survive regeneration; `exclude_from_stats` honoured.
- [ ] New trades reconstruct with **zero** manual log-joining (geometry + plan
      numbers + close facts all from the stream).

## Open questions

- Journal output format — Markdown matching the existing demo journal, or a
  structured intermediate the journal is generated from?
- MFE/MAE source: periodic mark-to-market events vs. a post-hoc candle join
  (mirrors the [event-schema open question](./event-schema.md#open-questions)).
- Spreadsheet write path — does the renderer write the sheet directly (Sheets
  API), or emit rows the operator imports?
