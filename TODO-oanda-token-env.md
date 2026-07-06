# TODO — native worker accepts OANDA_TOKEN (fix `oanda login failed`)

## Problem

`tv-arm-staging --register-plan --broker oanda --account-id=m-and-w` →
worker HTTP 500 "oanda login failed".

Root cause is NOT the `src/lib.rs` wasm stub (dead code in the deprecated CF
crate). The **live native worker** (`worker/`) reads `secrets.oanda_api_key`
in `broker_factory::acquire_oanda`. `Secrets::from_env()`
(`worker/src/secrets.rs`) reads **only `OANDA_API_KEY`**, but the docs
(CLAUDE.md / README) and the rest of the codebase (`tv-arm/src/spread.rs`,
`replay-candles`) use **`OANDA_TOKEN`** (with `OANDA_API_KEY` as fallback).
The running staging/dev workers were booted with `OANDA_TOKEN` set and
`OANDA_API_KEY` unset → `secrets.oanda_api_key = None` →
`BrokerError::MissingOandaApiKey` → 500.

## Plan

- [x] Diagnose: env-var name mismatch (`OANDA_TOKEN` vs `OANDA_API_KEY`).
- [x] Fix `Secrets::from_env()` to read `OANDA_TOKEN` first, fall back to
      `OANDA_API_KEY` (mirror `tv-arm/src/spread.rs` precedence).
- [x] Update the secrets.rs module doc table.
- [x] Add tests: `OANDA_TOKEN` accepted; `OANDA_TOKEN` wins when both set;
      `OANDA_API_KEY` still works as fallback.
- [x] `cargo test -p trade-control-worker --lib secrets::` (7 ok), clippy, fmt.
- [x] Merged fix → `main` and → `staging`; both pushed.
- [x] Rebuilt `trade-control-worker` (release) + redeployed `-dev` and
      `-staging` CLIs.
- [x] Restarted BOTH workers from canonical on-disk sources (keys from
      `~/.config/trade-control/{key,admin-key}.hex`, `OANDA_TOKEN` from
      `~/.zshenv` via `zsh -l`). Logs: `~/.local/state/trade-control/{dev,staging}-worker.log`.
      New PIDs: dev 3164944, staging 3170130. Staging reloaded its 6 live
      plans from Postgres — no state lost.
- [x] Verified end-to-end on both: the same arm command no longer 500s on
      "oanda login failed"; it now reaches the trade-quality gate and returns
      a legitimate 422 (SL too close to spread, R < min_r 1.00). OANDA login
      works.
- [ ] Advance parent submodule pointer after merge to main (final step).

## Note

The 422 is a real property of THIS setup (EUR/CHF SL drawn too tight for the
spread), not a bug — a wider-SL setup passes the gate. Separate from this fix.

Reboot still kills these `nohup` workers (no systemd locally) — restart
manually after a reboot.
