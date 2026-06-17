# Engine tick-bundle recording + replay — TODO

Branch `feat/engine-tick-replay` (worktree `../tcwh-engine-replay`), cut from `main`.
Plan: `~/.home-claude/plans/glowing-dancing-sketch.md`. Brief: `ENGINE_REPLAY_RECORDING_PROMPT.md`.
No deploy to staging/prod. Each step its own green commit (test + clippy + fmt + wasm where glue touched).

## (a) core pure types — DONE (commit pending)
- [x] Relocate `FiredIntent` + `PlanEval` defs → `core/src/plan_eval.rs`; re-export from engine lib + evaluate
- [x] Add `Serialize, Deserialize` to: `Candle`, `LatchedSignal`, `FiredIntent`, `PlanEval`
      (dropped `PartialEq` — `Intent` doesn't derive it; compare via serialized JSON instead)
- [x] New `core/src/tick_bundle.rs`: `TickBundle` + `DispatchOutcome` + `KvTickTransition` + `r2_key()`
- [x] serde round-trip unit test (JSON fixture → parse → reserialize → compare serde_json::Value)
- [x] Gate: core 504 + engine 28 tests green; clippy clean; fmt; full workspace builds

## (b) MemStateStore behind test-support feature
- [ ] `mod memstore` #[cfg(test)] → #[cfg(any(test, feature = "test-support"))]
- [ ] `test-support = []` in core/Cargo.toml [features]; `pub use memstore::MemStateStore` gated
- [ ] Gate: cargo test -p trade-control-core --features test-support → commit

## (c) record_tick_to_r2 + ScheduleContext threading — SHADOW ticks only
- [ ] src/cron.rs: `_ctx` → `ctx`; pass `&ctx` to run_engine_tick
- [ ] thread `ctx: &ScheduleContext` → run_engine_tick → tick_one
- [ ] new src/tick_recording.rs: record_tick_to_r2 (mirror record_to_r2, fail-soft, wasm-cfg)
- [ ] assemble TickBundle in tick_one; gate emission on `plan.shadow`
- [ ] R2 key: ticks/<date>/<tick_ts>-<trade_id>.json
- [ ] Gate: workspace test + clippy -D warnings + fmt + wasm build → commit

## (d) extend to live ticks
- [ ] drop shadow gate; populate dispatch_outcomes from each dispatch_fired result
- [ ] Gate: workspace test + clippy + fmt + wasm → commit

## (e) native replay CLI
- [ ] cli/src/replay.rs: `trade-control replay <path-or-r2-key>`
- [ ] re-run evaluate_plan, diff fired/new_state/done, non-zero exit on mismatch
- [ ] cli deps: trade-control-core test-support, trade-control-engine, serde_json
- [ ] Gate: test + clippy + fmt → commit

## (f) broker-simulator for fill replay
- [ ] implements Broker trait; candles from recorded new_candles, not refetched
- [ ] replay also diffs dispatch_outcomes
- [ ] Gate: test + clippy + fmt → commit

## Cross-cutting
- [ ] CHANGELOG + vNN tag per crate when (a)/(b), (c), (e) green
- [ ] README event-format note: tick-bundles under ticks/ prefix (sibling to req/)
