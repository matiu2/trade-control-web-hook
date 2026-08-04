# TODO: fixture sub-bar storage (step 2) — DONE

`--save` now freezes `sub_bars.json`: exactly the finer candles the zoom
consulted, and nothing more. No ambiguous bar ⇒ no file (byte-identical to a
pre-feature fixture). A fixture whose verdict needed a zoom now reproduces it
offline instead of degrading to the pessimistic stop.

Stale fixtures **refetch** rather than degrade: `lazy_zoom::FixtureSubBars`
reports a window outside the saved extent as a miss, and `run_frozen` refetches
just that window. Verified: deleting `sub_bars.json` refetched 2 windows / 103
bars and reproduced the identical −2.00R verdict.

Coverage is judged by the saved candles' **extent**, not per-candle — an illiquid
cross legitimately has minutes with no candle, and splitting the span at each one
would refetch it forever. Known tradeoff (gap between two distant saved windows
reads as covered) is pinned by a named test; it fails conservatively.

The corpus unit test stays **fully offline** (serves saved sub-bars, never
refetches) so `cargo test` needs no credentials; only `--test-mode` refetches.

Sizing that drove this: whole-window M1 ≈ 416 MB for the corpus, "after the right
shoulder" ≈ 111 MB, what the zoom can actually consume ≈ **1.1 MB**.

- [x] `sub_bars.json` (optional, absent when empty; stale file removed on re-save)
- [x] `FixtureSubBars` — serves saved bars, records uncovered windows
- [x] `run_frozen` refetches misses; corpus test stays offline
- [x] 271 tests green; 19/19 fixtures `--check`; clippy clean; fmt
- [x] Mutation-checked (one mutation initially SURVIVED and exposed a weak test
      + a real coverage bug — both fixed)

---

# TODO: lazy two-pass sub-bar zoom (step 1) — DONE, deployed to staging as v128

## Why

The sub-bar zoom (PR-2) eagerly pulls a FINER series (M1 under an H1 plan)
across the WHOLE coarse window before the sim runs. But the zoom is consumed
only when a post-fill bar straddles BOTH SL and TP — and the exit loop `return`s
on the first such bar, so each entry triggers **at most one** zoom.

Measured on `replay-fixtures/cad-sgd-h1-2026-07-21`:

- coarse window 2026-07-21 14:00Z → 2026-07-29 08:00Z = 186 H1 bars
- eager M1 pull = **11,160 M1 slots**
- entries in that cell = 2 ⇒ worst case **2 zoomed bars = 120 M1 slots**
- the EUR/USD cell has **zero** entries ⇒ pulls 11k, consumes **none**

Those M1 slots are also where candle-cache re-fetches: an illiquid cross has
scattered non-ticking minutes (CAD/SGD 2026-07-22: 200 missing of 1440), and a
partial broker response leaves them unmarked, so they re-fetch every run.

We are **NOT** fixing that by negative-caching. Permanent holes bit us before —
a misclassified "permanent" error left candles that were never retried again and
corrupted trades. Lazy fetching only ever asks for LESS; it never records
"don't ask again".

## Approach — two-pass, sim stays sync + deterministic

`SubBars` is deliberately a **sync** one-method trait so no async is threaded
through the sim. Keep that.

1. **Pass 1**: run the sim with `NoZoom`, collecting the bars that came back
   ambiguous (the `(true, true)` straddle arm).
2. **Fetch**: pull the finer series ONLY for those bars' windows.
3. **Pass 2**: re-run with a provider populated from that narrow fetch.

Rejected: a blocking fetch inside `sub_bars` — smaller diff, but puts I/O in the
sim hot path and makes it non-deterministic.

## Steps — DONE

- [x] Sibling worktree (`../trade-control-web-hook-lazy-zoom`) so `../`
      path-deps still resolve
- [x] Tests first, then **mutation-checked** (green proves nothing on its own).
      Three mutations, each caught:
      1. straddle arm no longer zooms → 3 of 4 tests red
      2. zoom window end halved (`bar_len / 2`) → 3 of 4 red
      3. `WindowSubBars` drops its sort → parity test red
- [x] `RecordingSubBars` — pass 1 serves nothing (so it IS `NoZoom`) and records
      the windows the sim asks for. Derived from the sim ITSELF, not a second
      hand-written straddle test that would have to agree with it by hand.
- [x] `WindowSubBars` + `coalesce` (adjacent/touching windows → one fetch)
- [x] `fetch_windows` — narrow, fail-soft per window
- [x] Rewired the driver eager pull → two-pass lazy; `replay::run` now takes an
      `Option<Box<dyn SubBars>>` provider instead of a candle slice
- [x] Removed the eager `with_sub_bars(Vec<..>)` rather than leaving a second,
      wasteful way to do the same thing
- [x] 264 unit tests green; 19/19 fixtures `--check` green; clippy clean (the 2
      remaining warnings are pre-existing, in files this branch never touched);
      fmt run

## Measured on real broker data (CAD/SGD H1, 2026-07-21 → 07-29)

| | eager (before) | lazy (after) |
|---|---|---|
| replay wall-clock | **80.2 s** | **0.054 s** |
| `--save` wall-clock | **88.7 s** | **0.050 s** |
| broker fetches | **258** | **0** |
| M1 bars pulled | **22,393** | **0** |

Report output byte-identical (`diff` clean), and the `--save` fixture
byte-identical across `candles.json` / `expected.json` / `plan.json`.

Zero fetches because **no fixture in the corpus has an ambiguous bar** — so no
finer candle could have changed any outcome. Pass 2 was verified separately by
temporarily forcing ambiguity: 2 windows → 2 fetches → 103 M1 bars (vs 258 /
22,393). That probe was reverted.

## Invariant this must not break

`NoZoom` callers (engine unit tests, fixture re-sim, `simulate_fill*` test entry
points) stay byte-identical. The zoom only ever REDUCES ambiguity; the
pessimistic stop is the floor.

## Note for whoever adds sub-bar support to FIXTURES

Fixtures do **not** store finer bars (`fixture::save` freezes plan + COARSE
candles + meta + expected; the frozen re-sim runs with no zoom at all). Don't
freeze whatever `lazy_zoom` happened to fetch — the recorded windows are a
function of the *current* strategy/bracket/widen state, so a saved narrow series
would be missing exactly the bars a changed strategy asks for, and the zoom would
silently degrade to the pessimistic stop on a fixture that looks complete.
Offline zoom needs the finer bars for the whole window, as an explicit (larger)
artifact. Documented in `lazy_zoom.rs`'s module docs.

---

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

## 5. Review follow-ups — [x]

An independent review of items 1–4 found three real defects (all fixed +
verified by execution, `aeededb`), then two more in `derive_table_name`
(`83becae`, `f333079`):

- [x] **The cache was never actually shared.** Switching to `postgres-storage`
      fixed concurrency but the table name derives from `cache_dir`, which was
      left at `CacheConfig::default()` — so replays used a private 54MB table
      while `candle_cache_tradenation_bid_ask` (431GB) sat unused beside it.
      Now per-broker, matching `broker-trait`'s `broker_env`. Also removes a
      latent cross-broker key collision.
- [x] **Classifier was wrong in BOTH directions.** Substring-matching the error
      chain meant an ENOENT on a dropped mount read as bad-input (recorded as a
      permanent result — the dangerous direction), while a typo'd instrument
      said "unsupported instrument" vs the marker's "unknown instrument" and was
      retried forever. Now a typed `BadInput` marker. NB it must sit at the head
      of the chain: eyre's `wrap_err` erases the context's concrete type.
- [x] **`--check` mismatch exited 3** (retry forever) for a deterministic
      regression verdict → new exit 5.
- [x] **`Net R:` missing on `--simulate false`** (reachable by design since the
      flag became overridable) → always emitted, `n/a` when nothing simulated.
- [x] **Over-long table names silently lost an index.** Postgres truncates at 63
      chars with only a NOTICE; this backend appends up to 26, so from a 38-char
      base the two index names collide after truncation and the 3-column index
      is **never created** — invisible full-scan cliff on a 431GB table, and
      unfixable downstream since it arrives as a SQLSTATE
      `execute_idempotent_ddl` tolerates. Now rejected (cap 37).
- [x] **Unusable paths fell back to a real table.** `/`, `___`, `123`, and a
      missing final component all returned the literal `candle_cache` —
      production data, not a sentinel. Now an error.

Deliberately NOT fixed (documented instead): the sanitizer is many-to-one, so
`candle_cache-oanda` and `candle_cache_oanda` still collapse to one table.
Inherent to deriving a name from a path.
