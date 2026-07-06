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
- [ ] Operational: rebuild + restart staging worker (deploy-staging.sh) so the
      running worker picks up the new binary; the already-exported `OANDA_TOKEN`
      then resolves. Or, as an immediate unblock without a rebuild, restart the
      current worker with `OANDA_API_KEY` also exported.
- [ ] Commit + push; advance parent submodule pointer after merge to main.
