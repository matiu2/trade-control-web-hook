# TODO — fix Bug #11: re-entry while prior position still open — DONE

Root cause (confirmed from 18-Jun staging logs): a still-open TN position
resolved to `Unknown` because TN's bracket fill drifts the live order id
away from the stored entry order id; the gate treated `Unknown` as "done"
and stacked a duplicate. The `1→0` sweep oscillation was a red herring
(dev + staging both logging into one file).

Fix shipped (three layers, all done):

- [x] (1) `Unknown` → fail-safe reject (412) in `retry_gate::evaluate`
      (`rejected: prior-attempt-unknown`).
- [x] (3) `compute_attempt_state` step 2 also matches `Position.position_id`.
- [x] (2) Independent open-positions backstop before placement
      (`rejected: trade-already-open (backstop)`; transient → 503 fail-safe).

- [x] cargo test green (224 passed)
- [x] cargo clippy clean
- [x] cargo fmt
- [x] CHANGELOG v44 entry (README: gate semantics are internal; no operator
      surface to change beyond the new reject strings, captured in CHANGELOG)
- [ ] commit + push; tag v44; advance parent pointer
