# Bug #15 fix + KV TTL model + `plan purge` command

## Root cause (verified)

Bug #15 (GBP/USD inverse-H&S `too-low` veto fired on reversals twin, not the
experimental twin) is **not** an engine-evaluation bug. `evaluate_plan` fires
`too-low` correctly against the real feed (see `engine/tests/bug015_repro.rs`,
3 passing tests). The real cause is the **`plan-state:` KV row's ~1-day flat TTL**
(`put_plan_state`, `plan_state_expires_at = now + 1 day`). When it ages out (or
KV read-misses) while the plan is still live, the next cron tick reads `None`,
**re-seeds** (`tick_one → seed_first_tick → seed_plan_state`), jumps the
watermark to "now" and fires nothing — silently skipping any cross in the gap.
The wall-clock `trade-expiry` survives a re-seed; a price-cross veto does not.

This is the **unfixed half** of the 2026-06-23 TTL fix (`775092e`), which
de-TTL'd `plan:` and `archived-plan:` but left `plan-state:` on a flat TTL.

## KV storage TTL model (decided)

**No-TTL (persist until explicit purge) — the per-trade rows:**
- `plan:` — already no-TTL (775092e)
- `archived-plan:` — already no-TTL
- `plan-state:` — **CHANGE: drop the TTL** ← the bug-15 fix
- `entry-attempt:` — **CHANGE: drop the TTL**
- `order-body:` — **CHANGE: drop the TTL**

**Keep window-anchored TTL (expiry is the intended behaviour) — control/dedup:**
- `cooldown:` (expiry = cooldown ending)
- `veto:` / `prep:` (anchored to intent `not_after` via `veto_ttl_seconds`)
- `pause:` / `news:` / `spread-blackout:` / `blackout-hours:` (window close)
- `seen:` / `retry-fire-seen:` (dedup markers; unbounded growth if kept forever)

## Tasks

- [ ] **(1) Repro tests** — DONE: `engine/tests/bug015_repro.rs` (3 tests green).
- [ ] **(2) De-TTL `plan-state:`** — `put_plan_state` writes with no
      `.expiration_ttl` (like `put_trade_plan`). Drop `ttl_seconds` from the
      trait + KV/MemStore/test-fake impls + the 2 engine call sites
      (`engine.rs:301`, `:420`). Remove `plan_state_expires_at`/`plan_ttl` flat
      math. Test: state row written with no expiry (mirror
      `memstore_registered_plan_does_not_expire`).
- [ ] **(3) De-TTL `entry-attempt:` + `order-body:`** — same treatment;
      both are per-trade lifecycle rows. (entry-attempt currently TTLs to
      `expires_at`; order-body to a caller TTL.)
- [ ] **(4) Belt-and-suspenders re-seed guard** — in `tick_one`, only seed
      when the plan has genuinely never ticked. If `get_plan_state` is `None`
      but the `plan:` row exists, treat as a transient skip (log + return),
      NOT a re-seed. Closes the KV-read-miss path even with no-TTL.
- [ ] **(5) Control-event log** — every successful `set_*` on a TTL-keeping
      control row (`cooldown`/`veto`/`prep`/`pause`/`news`/`spread-blackout`/
      `blackout-hours`) also writes a durable, **no-TTL** event record keyed by
      `trade_id`: `{ name, set_at, ttl_seconds, computed_expiry, set_by_request }`.
      Gives journaling/debugging a set+expiry trail after the live row is gone
      (TTL expiry in KV is a passive delete — no event, no log, today). Read by
      `plan show`/journaling; cleared by `plan purge`. Append-only
      (`control-event:<scope>:<trade_id>:<seq>`).
- [ ] **(6) `plan purge <trade_id>` command** — deletes everything for ONE
      journaled trade:
      - KV: `plan-state:`, `archived-plan:`, `plan:`, `entry-attempt:*`,
        `order-body:*` (ids recovered from the attempts), trade-scoped
        `veto:`/`prep:`/`pause:`/`news:`, and `control-event:*`.
      - R2: `ticks/**/*-<trade_id>.json` (trade-keyed suffix — clean prefix match).
      - R2 `req/`: NOT trade-keyed → left to the time-based bulk purge (7).
- [ ] **(7) Time-based bulk purge** — `purge --older-than <1w|1month|…>`
      (manual). R2 `req/` and `ticks/` are date-partitioned
      (`<prefix>/<YYYY-MM-DD>/…`), so this is a clean **prefix list + delete by
      date**, no content scan. Sweeps both prefixes older than the cutoff plus
      any orphaned KV. This is what handles the body-hash-keyed `req/` bundles
      (which `plan purge` can't target by trade).
- [ ] **(8) R2 stays no-TTL** — confirm `req/` + `ticks/` writes carry no
      expiry (they don't today). Records persist until a purge command removes
      them.
- [ ] **(9) README + CHANGELOG** — document the no-TTL model, control-event log,
      and both purge commands.
- [ ] clippy + fmt; dev deploy to verify; advance parent pointer on merge.

## Note — R2 key shapes (why two purge commands)

- `ticks/<date>/<ts>-<trade_id>.json` — trade-keyed → `plan purge <id>` targets
  by suffix.
- `req/<date>/<ts>-<request_id>.json` — `request_id` is a body+headers hash, NOT
  the trade_id → cannot be targeted per-trade by prefix. Swept instead by the
  date-based bulk purge (both prefixes are date-partitioned).
