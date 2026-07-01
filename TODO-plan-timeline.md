# TODO — `trade-control plan timeline <trade-key>`

Event-reconstruction subcommand. Replaces the CF+R2 half of the old
`trading-tax-tracker timeline --merged` tool, sourced from the local
worker's Postgres `request_records` instead of Cloudflare R2 + CF logs.

Decisions (locked with user):
- **Home:** `trade-control plan timeline` subcommand (mirrors `plan show`).
- **Transport:** HTTP to the worker (NOT direct PG) — PG will live in
  Oracle Cloud eventually; the CLI must stay DB-agnostic.
- **Source:** `request_records` only for v1. `tick_bundles` (cron
  engine-side fires) is a follow-up.
- **Scope:** `WHERE trade_id = $1 ORDER BY ts`. Exact trade_id match —
  "all related events from a single trade". (intent_id-prefix recall
  dropped.)
- **Rendering:** slim single-source — one section per RequestRecord in
  ts order (header: ts · outcome · status · request_id; then its
  `logs[]`). `--json` dumps raw records. No CF/R2/S3/TV-CSV/broker.
- **Tax/accounting (P&L, fees, slippage):** SEPARATE command, later,
  same repo. Broker-sourced, not part of timeline.

Design (no `StateStore` trait change — timeline read is PG-specific,
like the `PlanPurge` arm):

- [x] `Action::PlanTimeline` in `core/src/intent.rs` (+ classification
      in `core/src/dispatch/action.rs` non-broker list + MissingTradeId
      required-set + prompts.rs no-questionnaire arm).
- [x] Made `RequestRecord`/`LogLine` `Deserialize` (was Serialize-only);
      `LogLine.level` → `Cow<'static, str>` so the write path keeps its
      `&'static` literals while the read path can deserialize.
- [x] `request_records_for_trade(pool, trade_id)` in
      `worker/src/recording_pg.rs` (next to `record_request`).
- [x] Worker-local handler arm `handle_plan_timeline` in
      `worker/src/http.rs` — reaches `store.pool()`, serializes
      `Vec<RequestRecord>` to YAML, `record_seen` for replay parity.
- [x] CLI: `PlanCmd::Timeline(PlanTimelineArgs)` + `--yaml`/`--json`/`--verbose`.
- [x] CLI: `build_plan_timeline_intent` (mirror `build_plan_show_intent`).
- [x] CLI: `run_plan_timeline` + `format_plan_timeline` renderer.
- [x] Tests: 2 core validation/wire-string, 3 CLI renderer, 1 core
      record round-trip. All green (764 core / 334 cli / worker suites).
      SQL read left to integration (no PG unit harness, matches existing).
- [x] README: documented the new action + subcommand.
- [x] clippy + fmt clean (incl. wasm lib under wasm32 target).
- [ ] commit on `feat/plan-timeline`; then user deploys dev worker to pick
      up the new worker route (CLI needs rebuild for the subcommand).

Follow-ups (not this change):
- `tick_bundles` leg (engine-side fires by correlation_id).
- separate broker-sourced tax/accounting command.
- rename `replay-candles` → `replay-events` (virtual events), pairing
  with this `timeline`/rebuild-events (actual events).
