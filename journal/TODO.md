# TODO: journal --new-tv (chart backend flag)

Operator ask: journal's `l` (load) key currently always drives TradingView via
tv-mcp. Add a `--new-tv` startup flag so `l` instead points at a local-chart
instance (http://127.0.0.1:8790 by default).

## Plan

- [x] Investigate how local-chart accepts instrument/timeframe on load.
      Finding: `static/index.html` reads NO URL params at all (confirmed by
      grep — zero `URLSearchParams`/`location.search` in the whole file).
      `#instrument` is a plain text input, `timeframe` a JS module-scoped
      var, both set only by hand (typing / clicking a `#tfs` button) or by
      `window.__walk.goto(i)` (index-based, not usable here). No server-side
      "current view" state exists either (`src/http.rs` confirmed — the
      whole app is stateless per-request except drawings-on-disk).
      DECISION: added a small, isolated, append-only URL-param bootstrap to
      local-chart's `static/index.html` (`?instrument=&tf=`), on branch
      `feat/new-tv-url-bootstrap` in a **separate** local-chart worktree
      (`../local-chart-new-tv-bootstrap`, pushed). Verified end-to-end with
      Playwright against a real running instance on a throwaway port. Kept
      the diff to a single appended `<script>` block at EOF specifically
      because two other agents were concurrently editing that file
      mid-body — appending at EOF cannot textually collide.
- [x] `--new-tv [URL]` flag on `journal`'s `Args` (clap, in `main.rs`),
      defaulting the URL to `http://127.0.0.1:8790` when the bare flag is
      given.
- [x] `ChartBackend` enum (`TradingView` / `LocalChart { base_url }`) threaded
      explicitly into `App::new`, stored on `App`, passed down to
      `start_load_tv` → `jobs::spawn_load_tv` → a new
      `tv::load_chart_backend(backend, instrument, broker, granularity)`
      that dispatches to the existing TV path or the new local-chart path.
      Kept `tv::load_chart` (TV-only) working byte-identical as the default.
- [x] Local-chart backend function: resolve instrument via
      `instrument-lookup` (OANDA-style id — local-chart's own convention,
      confirmed against `local-chart/src/symbol.rs`'s `resolve_symbol`
      docs), map granularity to local-chart's lowercase timeframe tokens
      (`m15`/`h1`/`h4`/`d`/`w`), and open
      `<base_url>/?instrument=<id>&tf=<tf>` via `opener::open` (already
      knows how to open a URL — reused as-is).
      Preserve fail-open asymmetry: any doubt (unresolvable instrument,
      unknown granularity) still opens SOMETHING (falls back to the bare
      stripped-separator instrument / default tf) rather than refusing —
      matching tv.rs's "answers false on any doubt" spirit (never leaves the
      operator stranded with no action at all).
- [x] Already-there fast path for local-chart: no server-readable "current
      view" exists (confirmed above), so there is no honest way to detect
      "already showing this" server-side. Decision: `load_chart_local`
      always opens/navigates (returns `Ok(false)` — "did load") rather than
      fabricating a match. Documented as a known, deliberate asymmetry from
      the TV path (never silently claim already-there when we cannot know
      — that direction is the SAFE one, matching the fail-open rule).
- [x] UI indicator: show the active backend in the journal footer so `l`
      behaviour is never a surprise.
- [x] Tests: unit tests for the flag parsing, the backend dispatch (observe
      args/URL rather than driving a real browser), the local-chart mapping
      function, and the always-false-already-there contract. TradingView
      path proven unchanged via existing `tv.rs` tests (untouched).
- [x] Mutations: invert the flag's sense; make the local backend fail
      CLOSED (claim already-there) on doubt; drop granularity from the
      local-chart mapping (vary both halves).
- [x] `cargo test -p journal`, `cargo clippy --all-targets -- -D warnings`,
      `cargo fmt`.
- [x] Commit on this worktree's branch, explicit file staging, push. Do NOT
      merge to main, do NOT advance the parent gitlink.

## Notes

- Scoping doc `local-chart/SCOPING-tv-arm-on-local-chart.md` §3 explicitly
  endorses a `--chart-source`-style flag for tools that read a chart for
  context (journal's load is exactly this), as opposed to `tv-arm` itself
  (which must never branch at runtime because it produces a signed plan).
- `journal/src/cli.rs` is NOT the clap arg parser — that's `main.rs`'s
  `Args` struct. `cli.rs` is the shell-out wrapper for `trade-control`/
  `tv-arm`/`replay-candles` subprocess calls (confusing name, pre-existing).
- **External dependency**: this feature only works once `local-chart`'s
  `feat/new-tv-url-bootstrap` branch (pushed, NOT merged) lands on whatever
  branch the operator actually runs. Until then `--new-tv` opens local-chart
  at the right instrument but the bootstrap script isn't in the served page,
  so it lands on local-chart's own default (EUR_USD/H4) instead. `journal`'s
  own tests do not depend on that branch (all mocked/pure except one
  `#[ignore]`d e2e test that needs a real local-chart instance running that
  branch — see `tv/local_chart.rs::tests::e2e_real_url_against_a_running_local_chart_instance`).
- Real end-to-end proof (both halves): (1) headless Playwright against a
  throwaway local-chart instance, on the local-chart worktree's own branch,
  confirmed instrument+timeframe both apply from `?instrument=&tf=`,
  independently, plus correct no-op on a partial/bad pair. (2) journal's own
  `#[ignore]`d `e2e_real_url_against_a_running_local_chart_instance` test
  drives the REAL (non-mocked) mapping code — including a live
  `instrument-lookup` catalog resolution of a TradeNation-form instrument —
  against a running instance and confirms the built URL is genuinely served.
