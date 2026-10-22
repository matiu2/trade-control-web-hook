# Finish a long cTrader demo calibration

`finish-ctrader` runs or resumes the existing 90-day, all-supported-FX generator,
re-authenticates the current full supported FX catalogue and rejects subset
reports, strictly bakes a complete reviewed report, and runs release tests, strict Clippy
and formatting checks for the shared cache and its broker/core/CLI/tv-arm/worker
consumers. Any failure stops later steps and writes its reason to the status JSON.
There are no commits, pushes, tags, main-branch changes or production deployment.

Build all three sibling binaries from the workspace:

```sh
cargo build --release -p spread-baseline-gen \
  --bin generate-ctrader --bin bake-ctrader --bin finish-ctrader
```

Run from `trade-control-web-hook`:

```sh
target/release/finish-ctrader --account ctrader-demo \
  --report spread-baseline-gen/validation/ctrader-demo-90d-report.json \
  --status spread-baseline-gen/validation/ctrader-demo-finish.json
```

For a durable background job, run that command in a systemd user service with its
working directory set to this workspace. Parent directories for report/status
must already exist. The report is resumed if present; original account, symbols
and dates stay pinned. Window size defaults to 24 hours. Detailed per-window
progress is in the child generator's log; the status JSON records the current
completion phase and source fingerprints.

Deployment is disabled by default. Explicit `--deploy-staging` calls only
`./deploy-staging.sh`, after validation succeeds, requires the `staging` branch,
and verifies `trade-control-worker-staging.service` is active afterward. This
rebuilds/installs staging tools and restarts the demo worker. Existing broker
positions and account configuration are not changed by this command.

At launch the helper snapshots SHA256 inventories and repository HEADs for all
local dependency packages discovered through offline Cargo metadata, including
build scripts, configuration, toolchains and included assets. Existing drafts
are preserved exactly. Changes to those bytes or HEADs before baking/deployment
abort completion. Calibration report artifacts are excluded; documentation is
pinned too; the generated cTrader table is checked separately before and after the
helper replaces it. Empty advisory-lock sidecars remain for stable lock identity;
the OS releases active locks if a process exits or crashes.

The helper requires the three binaries to match the reviewed source you launch.
If interrupted, rerun it: completed candles/profiles remain cached. A failed bake
or validation leaves its evidence/status available and performs no subsequent
deployment. After a successful finish, review and commit only the generated
cTrader table, calibration evidence and validation receipt; unrelated drafts stay
outside those commits.
