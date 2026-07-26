# TODO: auto TP-resistance band — same width as a drawn S/R line

**Rule (operator 2026-07-24):** the auto TP-resistance band is currently HALF the
width of a drawn S/R band for the same `pct` (drawn = `±pct` = 2·pct total; auto
TP = one-sided `+pct` = 1·pct total). Fix: move the band's **center** to the
approach-side offset `TP ± pct` and build a normal `±pct` band around it — so the
band's edge lands exactly on TP (a clean run to TP is unaffected, never extends
PAST TP) but its total width now equals a drawn line's, reaching further up the
approach side to catch a reversal short of TP. Keep the default `pct` unchanged
(operator: this won't fix the XAU_XAG 0.25%-short reversal — that's a separate
width-tune decision).

Geometry (pct as fraction):
- Short (falls into TP from above, reversal ABOVE TP): center `TP·(1+pct)` →
  band `[TP, TP·(1+pct)·(1+pct)]` → edge (lo) = TP, reaches up the approach.
- Long (rises into TP from below, reversal BELOW TP): center `TP·(1-pct)` →
  band `[TP·(1-pct)·(1-pct), TP]` → edge (hi) = TP.

## Steps
- [x] 1. `tv-arm/src/pipeline.rs::tp_resistance_band`: center at approach-offset
      `TP·(1±pct)` + normal ±pct band. Far edge = TP; near edge reaches 2·pct.
- [x] 2. Updated far-edge tests: edge still TP, new near edge asserted.
- [x] 3. New test `tp_resistance_band_matches_a_drawn_sr_line_width` (width ==
      drawn band, ~2× the old one-sided).
- [x] 4. `hs_default_adds_tp_resistance_band` still green (edge still == TP).
- [x] 5. tv-arm 263 tests green; clippy clean; fmt.
      (XAU_XAG short: band 68.324→[68.324, 68.461], was [68.324, 68.392].)
- [ ] 6. CHANGELOG vNN; commit+push; merge staging + redeploy; parent pointer.
- [ ] 7. (still queued, separate) uk100 fixture rebless.

---

# TODO: postgres candle cache + loud replay failures

Source: `~/projects/the-trading-academy/books/demo-journal/DEV-BRIEF-postgres-candle-cache.md`

Goal: stop concurrent replays silently corrupting batch results, and make the
failure modes loud enough that a driver can tell "retry this" from "record this
result" from "fix your input".

## 1. Switch `cli` to postgres-storage — [x]

- [x] `cli/Cargo.toml:15` — add `features = ["postgres-storage"]`
- [x] **Not a one-line change** — two blockers the brief didn't see:
  - [x] **Dependency conflict.** `cli` shares a workspace with `journal`
        (`rusqlite` → `libsqlite3-sys` 0.34). sqlx's default features pull
        `sqlx-sqlite` → `libsqlite3-sys` 0.28; only one package per graph may
        set `links = "sqlite3"`, so the build failed to *resolve*. Fixed with
        `default-features = false` (Postgres is the only driver used) + bumping
        candle-cache `^0.8` → `^0.9` so the graph unifies on the single sqlx the
        worker already required. sqlx 0.9's new `SqlSafeStr` bound then needed
        `AssertSqlSafe` on 27 dynamic-SQL sites — audited: table names are
        sanitized to `[a-zA-Z0-9_]`, all values are `.bind()`-ed.
  - [x] **A second concurrency bug, exposed once ReDB was gone.**
        `CREATE TABLE IF NOT EXISTS` is not atomic against a concurrent
        creator, so parallel cold starts collided in the Postgres catalog
        (`23505 duplicate key … pg_type_typname_nsp_index`). Same silent-cell-
        loss pathology, different layer. Fixed by `execute_idempotent_ddl`,
        which treats `23505`/`42P07`/`42710` as "someone else won the race" and
        propagates everything else.
- [x] Verified: 6 concurrent cold-start replays → **3/6 survived before the
      race fix, 6/6 after** (same command, tables dropped between runs).

Not touching the sibling worktrees (`trade-control-web-hook-*`,
`trade-control-{tick-precision,retest-gate,news-bars-old}`) — other agents are
live in them; they inherit this when they merge from `main`.

## 2. `--annotate` last-wins — [x]

`ArgAction::Set` with no `overrides_with` makes a repeated `--annotate` a clap
error, so `tv-arm … replay -- --annotate false` cannot turn off chart drawing.
`build_argv` already appends passthrough last (and tests the ordering) — only
the clap definition was wrong.

- [x] `overrides_with` on all three `ArgAction::Set` bools — `--annotate`,
      `--simulate`, `--annotate-unfilled` (the latter two had the same latent
      trap; made consistent rather than fixing only the reported one)
- [x] Test `repeated_bool_flags_take_the_last_value`
- [x] Verified end-to-end: `--annotate true --annotate false` used to error
      with "cannot be used multiple times", now runs with annotation off

## 3. Loud failures — [x]

New `cli/src/bin/replay_candles/outcome.rs` owns the whole taxonomy.

- [x] Always emit a terminal machine-readable line, even on failure
      (`Done: false | error: <kind> | detail: … | Net R: n/a`). `n/a`, not
      `+0.00` — a sweep would average the latter in as a real trade.
- [x] Distinct documented exit codes (shown under `--help`, since drivers
      branch on them): `0` ran-to-completion incl. genuine 0R, `3`
      infrastructure (retry), `4` bad input (fix it); clap keeps its own `2`.
      Unrecognised errors classify as *infrastructure* — a wrong guess there
      costs one retry, the other way silently drops a cell.
- [x] `CANNOT REACH CANDLE CACHE at <url>: <cause>` on the cache-open path
- [x] Verified: success→0, bad input→4, unreachable DB→3, bad flag→2

## 4. candle-cache hardening — [x]

- [x] No-backend-feature `MemoryStorage` fallback is now a hard
      `CacheError::config` naming the feature to enable (was a `warn!`)
- [x] **Also found and fixed a second, worse silent degrade:** a `cache_dir`
      merely *containing* the substring "test" downgraded to in-memory, and it
      was NOT gated on `cfg(test)` — so a release run pointed at
      `/tmp/latest-run` or `/data/backtest` silently cached nothing and
      re-fetched everything. Now an explicit `CacheConfig::use_memory_storage`
      flag that `minimal()` opts into.
- [x] Fixed `examples/migrate_redb_to_postgres`, broken since
      `RedbStorage::new` gained a parameter (pre-existing; blocked
      `--all-targets`)

## Coordination with `feat/fixture-corpus` (other agent)

- **`--json` (brief's ask #5): THEIRS.** They're building it as part of the
  fixture corpus, designed to satisfy both docs — one object per fixture
  including on failure (`ok:false` + null outcome), batch continues past a
  failure, roll-up marks `← INCOMPLETE`. Don't build a second one.
- **`--annotate`: both of us fixed it, and the fixes compose.** They fixed the
  *injection* side (`build_argv` only injects a default when the passthrough
  doesn't already set it — which their `--arm-*` flags need anyway, since those
  are `requires = "save"`). I fixed the *parser* side (`overrides_with`), which
  also covers anyone calling `replay-candles` directly rather than via tv-arm.
  Verified: `main` merges into `feat/fixture-corpus` cleanly, both fixes survive,
  and the `postgres-storage` line resolves to my version as they asked.
- **`--start` strict-RFC3339 and a `--no-annotate` alias: THEIRS** (they're
  taking it with `tv-arm --spec-in`).
- Their point that `--test-mode` never touches candle-cache is correct — the
  corpus path was already parallel-safe. My storage fix is for the live-arm
  path, which is the one their sweep still runs once per trade.

## Deferred (not this session, nobody has claimed)

- `--cache-dir` warns/rejects when the path doesn't map to a sane table name
  (under Postgres `derive_table_name` turns it into a **table name**, not a dir;
  it sanitizes to `[a-zA-Z0-9_]`, so two different dirs can collide on one table)
