# Fix: registered plans must not time out — archive on close instead — DONE

## Root cause
`build_register_intent` (cli/src/control.rs) builds on `control_skeleton`, which
sets `not_after = now + CONTROL_TTL(5min)` and never overrides it for register.
Worker `handle_register` derived the live `plan:` KV TTL from
`(not_after - now) + 1h grace` → every registered plan expired from KV ~65min
after arming (proven via R2 dump 2026-06-23: every register `ttl≈3897s`).

## Decision
Plans never expire by TTL. They retire only via the engine's archive path
(`persist_plan_state` → `archive_plan` + `clear_trade_plan`) on a `done` eval
(terminal close / trade-expiry veto / window-close). That path already exists.

## Done
- [x] `put_trade_plan`: drop `ttl_seconds`; write live `plan:` key with NO expiry.
  - [x] core/src/state.rs trait signature (+ doc explaining why)
  - [x] src/state/kv.rs impl — omit `.expiration_ttl(...)` (mirrors `archive_plan`)
  - [x] core/src/state.rs MemStateStore impl — store via `NO_TTL_SECONDS` idiom
  - [x] fixed 3 other impls/stubs (retry_gate fake, lib.rs SeenSpyStore) + callers
- [x] `handle_register` (src/lib.rs): dropped `replay_ttl_seconds`; rlog now
      "persisted (no expiry)".
- [x] Tests:
  - [x] `memstore_registered_plan_does_not_expire` — expiry is far-future, not minutes
  - [x] `memstore_clear_trade_plan_removes_live_but_keeps_archive` — archive path intact
  - [x] added test-only `MemStateStore::expiry_of` accessor
- [x] cargo test green: core 584, cli 247+34+13, tv-arm 141, tv-news 76,
      engine 53, worker 211. clippy clean, fmt clean, wasm32 check clean.
- [x] README updated (the `plan:` key persistence note).

## Follow-up (not in this change)
- `CONTROL_TTL` 5-min default is still correct for real control actions; left as-is.
- After merge: re-arm the staging setups against the fixed `tv-arm-staging`.
