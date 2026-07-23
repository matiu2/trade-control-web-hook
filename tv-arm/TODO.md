# TODO — tv-arm: register-plan / plan-out / replay → clap subcommands

Goal: turn the three top-level options (`--register-plan`, `--plan-out FILE`,
`--replay [args]`) into **strictly-exclusive** clap subcommands:

```
tv-arm register        # arm only (was --register-plan)
tv-arm plan-out FILE   # write JSON only (was --plan-out FILE)
tv-arm replay [args]   # replay only (was --replay [args])
tv-arm                 # (no subcommand) build+sign to disk only — unchanged default
```

Decided with user: **strictly exclusive** — no more `--register-plan --replay`
style combos in one invocation.

Sub-flags that belonged to those options move onto their subcommand:
- `--replace [id]` / `--update` (alias)  → under `register`
- `--shadow`                              → under `register`
- `<FILE>` positional                     → under `plan-out`
- trailing `[REPLAY_ARGS...]`             → under `replay`

## Steps

- [x] 1. Add `Command` enum (`Register`, `PlanOut`, `Replay`) as
        `Option<Command>` subcommand on `Args`; move the sub-flags in
        (`--replace`/`--update`/`--shadow` → `register`; `<FILE>` → `plan-out`;
        trailing args → `replay`).
- [x] 2. Add derived accessor methods on `Args` (`register_plan()`,
        `plan_out()`, `replay()`, `replay_args()`, `replace()`, `shadow()`)
        so pipeline.rs churn stays minimal.
- [x] 3. Update pipeline.rs call sites to the accessor methods.
        `effective_plan_out` kept (still needed: replay writes a temp path).
        `pick_prune_as_of` uses `register_plan()`.
- [x] 4. Update/rewrite args.rs + pipeline.rs tests for the subcommand surface.
        Global flags (`--as-of`) reordered before the subcommand in test argv.
- [x] 5. main.rs unchanged — `--print-completions` still a top-level flag,
        verified it still emits with the subcommand present.
- [x] 6. `cargo test -p tv-arm` (262 pass), `cargo clippy` clean, `cargo fmt`.
- [x] 7. Update README + deploy-lib.sh + replay.rs/replay_args.rs doc refs.
        CLAUDE.md had no direct old-flag refs. Mode-describing internal doc
        comments ("the --register-plan path") left as-is to avoid churn.
- [ ] 8. Commit + push.

## Behavior change to note

`replay` no longer arms — it only builds + replays (was: `--replay` chained
*after* arming). To arm AND replay, run `... register` then `... replay`
separately. This is the strict-exclusivity the user asked for.
