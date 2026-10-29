# MT5 Real full-account spread bake

- [x] Read the account catalogue through the pinned desktop bridge.
- [x] Resolve pip units and local spread schedules for all 64 native instruments.
- [x] Measure 62 instruments over 90 days; explicitly use five-day provisional gold/NAS100 profiles where older ticks are unavailable.
- [x] Add validated multi-account bake merging while preserving competition profiles.
- [x] Validate 204 generator tests, 763 CLI library tests, Clippy and formatting; commit and push each completed change.

## Explicit shorter history for gold and NAS100

- [x] Measure XAUUSD and NAS100 over October 23–28: 4,095 validated M1 observations each.
- [x] Add opt-in supplemental report baking; preserve strict 90-day validation by default.
- [x] Test identity, catalogue, sample-quality and provenance checks; run Clippy and formatting.
- [x] Publish all 64 profiles with both source reports, their actual measurement dates and a comparison to the competition bake.
