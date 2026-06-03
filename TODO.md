# TODO

## Active — Consolidated close-on-reversal + first-class candle-quality gates

**Bug observed (2026-06-03):** GBP/NZD demo entry SL didn't match any obvious
swing structure, and a closer audit of the close-on-reversal path showed two
deeper issues:

1. The Pine `Long Pattern` / `Short Pattern` plots (used as the
   close-on-reversal trigger) fire on **any** opposite-direction signal,
   golden or not — there is no operator-facing way to require "golden only"
   on a Close, because `Intent::validate` rejects `needs_golden: true` on
   any action ≠ `Enter` (`core/src/intent.rs:699`).
2. We have two separate alert basenames (`06-close-on-reversal`,
   `07-close-on-sr-reversal`) for what is semantically one operation
   ("close when a reversal candle prints inside a meaningful contextual
   window"). The two-alert split is artificial and produces awkward CLI
   plumbing.

### Design

Single `06-close-on-reversal` alert that fires when ALL of:

1. **Inside a configured window** — at least one of:
   - active news window for this `trade_id`, OR
   - current broker price inside a configured price band.
2. **Candle quality** — golden (default), or operator-overridden to
   confirmed-but-not-golden.
3. **(optional) `allow_close` script** — ad-hoc filter, symmetric with
   `allow_entry`.

Plus: promote `require_confirmation` from a script-only check on the
`allow_entry` body to a first-class `needs_confirmed: bool` field on
`Intent`, applicable to both Enter and Close. Symmetric with the existing
`needs_golden`.

### YAML shape (new form)

```yaml
v: 1
action: close
id: <trade_id>-close-on-reversal
trade_id: <trade_id>
instrument: ...
broker: ...
account: ...

# New consolidated gate (replaces require_news_window + require_price_in_ranges)
inside_window: [news, price]              # OR-composed; at least one must be set
price_bands: [[1.0950, 1.0970]]           # required when "price" is in inside_window

# Candle quality (default: golden only). Mutually exclusive.
needs_golden: true                        # default for reversal closes
# needs_confirmed: true                   # operator opt-in to "confirmed, not necessarily golden"

# Optional ad-hoc filter (symmetric with allow_entry)
# allow_close: |
#   <script expr>
```

### Field naming

- `inside_window: [news, price]` — list of window types under which the close
  is valid. List-implies-any (OR), same surface area as `requires_preps` but
  with opposite composition. Documented explicitly in the field doc-comment
  and the README. The two-axis metaphor: news is a time-window, price is a
  price-window.
- `price_bands: Vec<[f64; 2]>` — the data for the "price" window type.
  Required when `price` ∈ `inside_window`; rejected when it's not.
- `needs_confirmed: bool` — symmetric with existing `needs_golden`. Both
  rejected on actions ≠ Enter|Close.
- `allow_close: Tunable<bool>` — symmetric with existing `allow_entry`.

### Wire-compat / deprecation

- Old fields `require_news_window` and `require_price_in_ranges` stay
  working unchanged (the worker already OR-composes them via
  `evaluate_close_gates` at `src/lib.rs:596` — verified). They are marked
  deprecated in doc-comments. Old in-flight alerts continue to fire
  correctly.
- An intent cannot mix old and new forms; validate-time rejection.
- `07-close-on-sr-reversal` basename stays in the `AlertBasename` enum for
  inbound decode of in-flight alerts; CLI stops emitting it.

### Steps

- [x] **Step 1: worker — validation relaxation + new fields.**
  - Relax `Intent::validate` to allow `needs_golden: true` on
    `Action::Close` (currently rejected at `core/src/intent.rs:699`).
  - Add `needs_confirmed: bool` to `Intent`. Same shape as `needs_golden`.
    Validate-time: only valid on Enter|Close.
  - Add `inside_window: Vec<EventWindow>` and `price_bands: Vec<[f64;2]>`.
    `EventWindow` is an enum `News | Price` with kebab serde.
  - Validate-time on Close: if either of the new fields is set, the old
    fields must be empty (mutual exclusion). At least one window-type
    gate must resolve to a real check.
  - Tests: round-trip, mutual-exclusion rejection, missing-data rejection
    (`price` in `inside_window` without `price_bands`).
  - No worker dispatch changes yet — just types.
- [x] **Step 2: worker — Close dispatch consumes new fields + candle gates.**
  - Extend `run_close` (`src/lib.rs:480`) to evaluate the new
    `inside_window` field via the same `GateOutcome` machinery. New form
    routes to the same `evaluate_close_gates` outcome that the old form
    already uses (so the OR semantics are guaranteed identical).
  - Add `needs_golden` / `needs_confirmed` shell-check before the broker
    call. Extract the existing `needs_golden` check from
    `src/allow_entry_gate.rs:51` into a shared helper used by both
    Enter and Close paths.
  - Add `allow_close` script evaluation symmetric with `allow_entry`.
  - Tests: golden-only blocks confirmed-non-golden, news+price OR, both
    failing rejects, allow_close composes AND with the rest.
- [x] **Step 3: CLI — consolidate `06`/`07` builders.**
  - `build_close_on_reversal_alert` becomes the sole reversal-close
    builder. Accepts `inside_window` + `price_bands` derived from the
    `TradeSpec` `close_on_news` + `sr_reversal_ranges` deprecated input
    fields. `TradeSpec.needs_confirmed_close: bool` flips the candle
    gate from `needs_golden: true` (default) to `needs_confirmed: true`.
  - Deleted `build_close_on_sr_reversal_alert`. CLI no longer emits the
    `07-close-on-sr-reversal` basename (the enum variant stays for
    inbound decode of in-flight alerts; see step 2's wire compat note).
  - Test rewrites: the `06`/`07` split tests became one-alert tests,
    plus a new `needs_confirmed_close` test. 209 cli tests pass.
- [x] **Step 4: Python emitter — obsoleted via deprecation.**
  - The chart-arming frontend has already been ported from
    `scripts/tv_arm_hs.py` to the Rust `tv-arm` crate. The Python
    script hasn't been behaviourally touched since 2026-05-29
    (`7034cef add 07-close-on-sr-reversal`); subsequent work has all
    landed in `tv-arm/`.
  - `tv-arm/src/pipeline.rs::build_trade_spec` still populates the
    same input-side fields (`close_on_news`, `sr_reversal_ranges`)
    on `cli::TradeSpec`. Step 3's consolidated
    `build_close_on_reversal_alert` then routes those into
    `inside_window` + `price_bands` on the emitted intent — so the
    Rust path produces the new wire form transparently with no
    further changes.
  - Marked `scripts/tv_arm_hs.py` deprecated: module docstring
    banner, runtime stderr warning at top of `main()`, argparse
    description tag. Script still runs if invoked.
  - Memory updated: `tv_arm_rust_supersedes_python` flags this for
    future sessions.
- [x] **Step 5: README + per-commit doc sync.**
  - `close` action documented with the three gate layers:
    contextual-window (`inside_window` + `price_bands`, OR-composed),
    candle-quality (`needs_golden` / `needs_confirmed`, AND-composed),
    ad-hoc filter (`allow_close` Rhai script).
  - Deprecated-form note on `require_news_window` /
    `require_price_in_ranges` (still accepted for in-flight alerts).
  - Alert-basename table collapsed from 06+07 split to a single
    consolidated `06-close-on-reversal` row.
  - Chart-arming section renamed from `scripts/tv_arm_hs.py` to
    `tv-arm`; CLI example switched to `cargo run -p tv-arm --`.
    Deprecation callout points operators away from the Python script.
  - News-pair / S-R-line drawing-table rows updated to describe the
    consolidated alert behaviour.

### Open follow-ups (not blocking the bug fix)

- Investigate the **6-7 min lag** between H1 close and broker fill on the
  2026-06-03 demo entries — see `entry_fill_lag_after_h1_close` memory.
  Most likely TV alert-eval lag; confirm via Cloudflare log timestamps.
- Audit the **GBP/NZD long SL drawing** (2.27736) — looks like it doesn't
  match the right-shoulder structure on chart, possibly a fib-anchor
  drag error on the operator side, not a code bug. Re-check after
  next setup is armed.

## Done — encryption retired; HMAC signing is the only auth

The encrypted envelope path (ChaCha20-Poly1305 over a `v1.<base64>`
payload) has been removed. Auth is now HMAC-SHA256 only, over the
cleartext body, via the existing `core::sig` module and the
`sign` / `verify` CLI verbs.

What changed:

- `core::crypto` module deleted. `parse_key_hex` / `KEY_LEN` moved to
  `core::sig` since the byte format is shared.
- `Shell.payload` removed.
- `IncomingError::Decrypt` removed; `parse_and_verify` no longer
  branches by envelope shape — every body is signed.
- `Cmd::Encrypt`, `Cmd::Decrypt`, `EncryptArgs`, `DecryptArgs`,
  `run_decrypt`, `extract_payload_blob`, `wrap_in_envelope`,
  `build_yaml_template`, `build_yaml_control_body`, `encrypt_intent`
  all deleted from the CLI. `EndpointArgs.signed` flag gone (always
  signed).
- `ENCRYPTION_KEY` worker secret renamed to `SIGNING_KEY`. The diag
  routes' `X-Diag-Key` header now compares against `SIGNING_KEY` too.
- `chacha20poly1305` dropped from `core/Cargo.toml`.

Migration for the deployed worker:

1. `wrangler secret put SIGNING_KEY < ~/.config/trade-control/key.hex`
   (same bytes as the old `ENCRYPTION_KEY`).
2. Deploy.
3. `wrangler secret delete ENCRYPTION_KEY`.

TradingView alert bodies that still use the encrypted format (top-level
`payload: "v1.…"`) will fail with a sig error after deploy — regenerate
them via `trade-control sign`. The user confirmed no live alerts in
the pipeline carry the old format.

269 tests pass after the cut (16 broker-oanda + 81 cli + 158 core +
14 worker); clippy + fmt clean on host + wasm.

## Done — TradeNation session lifecycle (wasm-side login)

The worker re-authenticates itself via per-account credentials stored
in `TN_ACCOUNT_<NAME>` secrets — no external rotation needed. On
cached-session rejection, the next request transparently re-logs in
using the stored credentials and writes the new session to KV. Both
demo and live login paths run inside the wasm worker via the
`worker::Fetch` crate (`reqwest`'s wasm shim auto-follows redirects
and can't be used).

The pre-named-accounts cron shim (`scripts/refresh-tn-session.sh`,
`TN_SESSION_JSON` secret, `TN_DEMO_LOGIN_ID` / `TN_DEMO_PASSWORD`
globals) was retired alongside Step 5 below. If you have stale
`TN_SESSION_JSON` / `TN_DEMO_*` secrets in the Cloudflare deployment,
run `wrangler secret delete` for each — the worker doesn't read them
anymore.

## Active — First-class accounts

Lift account selection out of "one TN session per worker" and into
named records, so a single deploy can route different intents to
different broker accounts (demo / live, OANDA / TradeNation).

Security model: metadata in KV (no secrets), credentials in
Cloudflare Secret Store (one binding per account). KV-only exfil
yields no password material. See conversation log 2026-05-19 for
the design rationale.

Steps:

- [x] **Step 1: core account types & traits.** `core::account` module
      adds `AccountKind`, `AccountCaps`, `AccountMetadata`,
      `Credentials` (TradeNation/OANDA variants), `MetadataStore`,
      `CredentialsResolver`, and the bundled `AccountStore`. In-memory
      impls for tests. 39 new tests, no worker integration yet.
- [x] **Step 2a: admin routes.** Worker-side `KvMetadataStore` +
      `SecretCredentialsResolver` (wasm) + four routes under `/admin/`
      gated by an `X-Admin-Key` header backed by a new `ADMIN_KEY`
      secret (distinct from `ENCRYPTION_KEY`). Routes:
      - `GET    /admin/accounts`           — list as YAML
      - `POST   /admin/accounts`           — add (JSON body)
      - `DELETE /admin/accounts/<name>`    — remove from index
      - `POST   /admin/accounts/<name>/test` — verify metadata +
        credential secret + broker match (no broker login yet)
      `wrangler secret put ADMIN_KEY` required before deploying. The
      credential secrets follow the schema `TN_ACCOUNT_<NAME>` /
      `OANDA_ACCOUNT_<NAME>` (name uppercased, `-`→`_`); blob is
      the JSON serialisation of `core::account::Credentials`.
- [x] **Step 2b: CLI verbs.** `encrypt-payload account
      list / add / delete / test` subcommands wired through the admin
      routes. Auth is `--admin-key-file` (env
      `TRADE_CONTROL_ADMIN_KEY_FILE`), separate from `--key-file`. `add`
      prompts for credentials via `dialoguer::Password` and pipes the
      JSON to `wrangler secret put` over stdin (no argv leakage); use
      `--no-secret` to skip the wrangler step. `delete --purge-secret`
      also runs `wrangler secret delete` (requires `--broker` so the
      binding name can be computed locally). New CLI modules:
      `admin_client.rs` (HTTP) and `admin_secret.rs` (wrangler shell-out).
      77 cli + 5 cli-bin tests pass; clippy + fmt clean on host.
- [x] **Step 3: plumb `account:` into the intent.** `Intent` gains an
      optional `account: Option<String>` field
      (`skip_serializing_if = Option::is_none` for back-compat).
      `acquire_tn_broker` now takes `account: Option<&str>`; when set,
      it routes through `KvMetadataStore` + `SecretCredentialsResolver`,
      caches the session under `tn:session:<name>` (per-account, so
      multiple TN accounts don't fight over one slot), and uses the
      account's own credentials. Demo accounts use the existing
      redirect-chain login with the account's username/password. Live
      accounts return `None` with a clear log (step 4 wires the live
      login). Account-less intents keep the legacy path
      (TN_SESSION_JSON / TN_DEMO_LOGIN_ID). `/diag/fx` and
      `/diag/candles` accept an optional `?account=…` query param.
      CLI `encrypt`: `account` is now an optional prompted field on
      enter/close/invalidate/veto (the broker-touching actions); blank
      input skips it so the wire form stays minimal. Worker
      `/admin/accounts/.../test` now emits the lowercase wire-form
      `broker:` / `kind:` values to match `list`. 149 worker + 74 cli
      tests pass; clippy clean on host + wasm.
- [x] **Step 4: live login path** (`login_live` in `tn_login.rs`).
      Drives the JWT → auth0 → cloudtrade hops, then reuses the
      existing redirect-chain harvest on the cloudtrade one-time URL.
      Three new helpers, all wasm-side: `get_jwt` (POST
      `tradenation.com/signup/api/login` with JSON body), `pick_account_id_from_jwt`
      (GET `portal.cube.finsatechnology.com/auth0/user` with Bearer),
      and `get_platform_url` (POST `…/cloudtrade/login` with Bearer +
      `account_id`). The platform-bootstrap step rejects sessions with
      no OTS — live writes use the OTS as the request `key`, so a
      missing OTS would silently break trade time; better to refuse
      here. Account-picking logic (`pick_funded_account`) is factored
      into `tn_login_helpers.rs` so it's host-testable (the wasm-only
      `tn_login` module isn't reachable under `cargo test`); also a
      shared `truncate_for_log` for trimming TN error bodies in logs.
      Wired into `acquire_tn_broker_for_account` via a new
      `login_and_cache_live` helper that mirrors `login_and_cache_demo`;
      the cache/serialise tail is now a single `cache_and_open` to
      avoid duplication. 14 worker + 149 core + 74 cli tests pass;
      clippy + fmt clean on host + wasm.
- [x] **Step 5: retire legacy fallback.** Removed
      `acquire_tn_broker_legacy` and its three constants
      (`TN_SESSION_JSON`, `TN_DEMO_LOGIN_ID`, `TN_DEMO_PASSWORD`,
      `TN_SESSION_KV_KEY`). TN routing now requires an `account:` on
      the intent — without one, the worker returns 503 with a clear
      "missing account" error. `scripts/refresh-tn-session.sh`
      deleted; the named-account path auto-relogs on cached-session
      rejection so no external rotation is needed. README updated to
      reflect the new TN session story.
- [x] **Three-way sizing modes + dry-run on intent.** `Intent` gains
      three new optional fields, mutually exclusive with each other
      and `risk_pct`:
      - `risk_amount: Option<f64>` — fixed money risk per trade in
        account currency (e.g. `1.0` to "bet $1").
      - `size_units: Option<f64>` — literal position size (e.g.
        `0.01` for one micro-lot). Bypasses sizing math entirely.
      - `dry_run: Option<bool>` — resolve + log sizing inputs/output,
        skip broker call. Useful for verifying templates safely on
        a live account.
      `Resolved` carries a new `RiskBudget` enum
      (`Percent(f64)` / `Amount(f64)` / `Units(f64)`). Resolver
      rejects multi-set or invalid values at the edge.
      OANDA broker consumes all three modes: `Percent` and `Amount`
      go through `units_for_budget`; `Units` skips sizing but still
      enforces `MAX_RISK_PCT_PER_TRADE` by reconstructing the
      implied money risk (`units * stop_distance` ÷ equity). TN
      adapter rejects both `Amount` and `Units` for now with clear
      logs (upstream `broker-tradenation` still takes `risk_pct`
      only — bumping it is a separate pass). Dry-run short-circuits
      in the worker dispatch before `place_entry` and logs id /
      instrument / direction / entry / SL / TP / risk-mode /
      implicit R; works for both brokers. 279 tests pass (77 cli,
      167 core, 16 broker-oanda, 14 worker, 5 cli-bin); clippy +
      fmt clean on host + wasm.
- [x] **Step 6: extend `AccountCaps` with `min_position_size`.**
      Done. New optional `min_position_size: Option<f64>` field on
      `AccountCaps`, surfaced via `--min-position-size` on `account
      add`. Worker loads `meta.caps.min_position_size` from
      `KvMetadataStore` and threads it through `EntryRequest`. Both
      brokers (TN adapter + OANDA `place_entry`) enforce the floor
      against explicit `RiskBudget::Units(s)` — `s < min` returns
      `UnitsBelowMinimum` before the broker is called. `Percent` /
      `Amount` modes skip the client floor because they compute
      units after equity/FX lookup; the broker's own
      `UnitsBelowMinimum` covers them. 276 tests pass (16
      broker-oanda + 81 cli + 159 core + 20 worker); clippy + fmt
      clean on host + wasm.

## Done

- **`fx_rate` rewrite to use live chart prices** — landed and deployed
  as `broker-tradenation-v0.3.0`. The root cause of the `risk_amount
  must be positive and finite, got 0` sizing failures was that
  TradeNation's `GetMarketQuote` returns `Bid: 0` and `Ask: 0` for
  every market — live prices were originally pushed over a WebSocket
  which has been silent since 2026-04-27 (only sends a `connectResponse`
  frame, then rejects every envelope with `Invalid request`). The v0.2.0
  zero-guard made the failure visible but couldn't fix it.

  The fix: `fx_rate` now resolves the pair to a `market_id` via
  `resolve_market` (unchanged) then fetches the latest 1-minute bid
  and ask candles from the unauthenticated
  `charts.finsatechnology.com/data/minute/{market_id}/{bid|ask}?l=1`
  endpoint, computing `mid = (bid_close + ask_close) / 2`. The chart
  endpoint needs no auth — only `Origin: https://chart-cfd.tradenation.com`
  and `Referer` headers — and works fine from inside wasm via
  `reqwest`'s wasm shim. Direct/inverse fallthrough preserved.

  Verified end-to-end via `GET /diag/fx`:
  - GBP/USD: 1.34182 (was 0.0)
  - USD/GBP: 0.7453 (inverse path)
  - EUR/USD: 1.16459
  - GBP/AUD: 1.87822

  Also extended the diag module with `GET /diag/candles?market_id=N&type=bid|ask&tf=minute&count=1`
  which hits the chart endpoint directly via `broker.client()` —
  useful for verifying a single market's chart data without involving
  `fx_rate`'s resolution logic. Worker bumped to
  `broker-tradenation-v0.3.0`, deployed to
  `trade-control-web-hook.msherborne.workers.dev`. 188 worker tests
  pass; wasm + host builds clippy-clean.

- **`GET /diag/fx` endpoint + upstream `fx_rate` zero-guard** —
  landed. New `src/diag.rs` module owns read-only diagnostic routes;
  `GET /diag/fx?from=GBP&to=USD` runs `tradenation_api::fx_rate`
  against the cached TN session and returns YAML with the resolved
  rate (or the error string). Auth via `X-Diag-Key` header whose
  value must equal the `ENCRYPTION_KEY` secret — re-using the
  existing key keeps secret management single-secret. Routing splits
  GET (diag) from POST (the existing encrypted-envelope handler)
  before body parsing.

  Why: TN's `fx_rate` was returning `Ok(0.0)` for `GBP/USD` during
  out-of-session hours, which flowed through to `stake_for_risk` as
  `risk_amount must be positive and finite, got 0` — diagnostic
  obscured behind two layers. The diag endpoint lets the operator
  reproduce the actual `fx_rate` output without firing a real entry.

  Upstream fix shipped as `broker-tradenation-v0.2.0`:
  (a) `fx_rate`'s direct branch now guards against zero mid
  (symmetric to the existing inverse-branch guard) and falls through
  to the inverse pair; if both fail it returns a `TradeError::Decode`
  carrying "direct FX pair X/Y has non-positive mid 0" — exactly what
  the operator needs to see.
  (b) `TradeNationBroker::client()` getter so consumers can call
  `tradenation_api::fx_rate` directly with the same `reqwest::Client`
  the broker uses (cookie / connection state stays consistent).

  Cargo.toml bumped to `broker-tradenation-v0.2.0`. Wasm + host
  builds clippy-clean.
- **`tracing` → `console_log` subscriber in the worker** — landed. New
  `src/tracing_console.rs` implements a minimal `tracing::Subscriber`
  (~110 lines) that formats events as `LEVEL target: field=value …` and
  routes `WARN`/`ERROR` to `worker::console_error!`, everything else to
  `worker::console_log!`. Installed once per worker instance via a
  `OnceLock` at the top of the fetch handler. Why: broker crates
  (notably `broker-tradenation`) log error detail through `tracing::warn!`
  / `tracing::error!`, but without a subscriber installed those events
  are silently dropped in wasm — so the worker's own lossy
  `entry failed: broker rejected the order` was the only breadcrumb. Now
  the actual TN rejection reason shows up in Cloudflare's request log.
  Step 1 of 2; step 2 is propagating the broker error string through
  `EntryError::OrderRejected(String)` once we've seen what TN actually
  says. Clippy clean on host + wasm targets.
- **`clear-prep` also forgets the prep's setter `seen:<id>`** —
  landed. Prep KV values now store `<rfc3339>|<setter_id>` instead of
  bare `<rfc3339>`, so the worker remembers which message-id set each
  prep. `clear_prep` returns the setter id; `handle_clear_prep` and
  `clear_named_preps` (the cascade-clear path triggered by a fresh
  upstream prep's `clears:` list) call a new `forget_seen` method that
  deletes `seen:<id>` and prunes the index. This means the operator
  can re-send the original prep message after `clear-prep` without
  hitting a 409 from replay protection. Wire format is
  forward-compatible — legacy bare-timestamp values still parse
  (empty setter_id, no seen-forget). 106 core + 67 cli + 5 cli-bin
  tests; clippy clean.
- **Per-instrument trade-expiry anchor (CLI-only)** — landed. New
  `cli::expiry` module persists a single `DateTime<Utc>` per instrument
  under `$XDG_CONFIG_HOME/trade-control/expiry/<INSTRUMENT>.txt`. The
  interactive flow asks for the anchor up-front when the operator
  declares a `veto` with `name: trade-expiry`, and stores whatever they
  enter (relative durations like `2d` accepted, ISO-8601 accepted).
  Subsequent prep/veto/enter prompts use the anchor as the default for
  `not_after` (read-only on `enter`), and prep/veto get a derived
  `ttl_hours` default (hours-from-now rounded up). A stale (past)
  anchor is silently dropped on load and the prompts fall back to the
  prior defaults (`8h` / `4`). Pure UX sugar — the worker neither sees
  nor cares about the anchor. Also fixed the save-as-template prompt
  so blank-Enter actually skips (previously the default value field
  meant Enter saved to `new.yaml`). 67 cli lib tests pass; clippy
  clean.
- **HMAC-signed cleartext wire format (parallel to encrypted)** —
  landed. New `core::sig` module: canonical form = fixed `v1-sig` tag,
  sorted schema-fingerprint of top-level keys (CSV), then `key=value`
  lines for every signed field, HMAC-SHA256 with `subtle::ct_eq` for
  verify. Shell fields (`close`/`high`/`low`/`time`) have their keys
  signed but their values excluded — so TradingView's `{{close}}` →
  number substitution doesn't invalidate the sig, but dropping a shell
  key does. Worker detects format by field presence (`sig:` vs
  `payload:`) and both paths run in parallel. CLI gains `--signed` on
  `encrypt`, `status`, `unlock`, `prep`, `veto`, `clear-prep`,
  `clear-veto`, plus a `verify` subcommand (mirror of `decrypt` for the
  signed path). Why: cleartext bodies show up in Cloudflare's request
  log so operators can read what TradingView sent without round-tripping
  through `decrypt`. Auth is unchanged (32-byte key, same key file).
  103 core + 55 cli + 5 cli-bin tests pass; clippy clean. End-to-end
  round-trip verified: signed encrypt → simulated TV substitution →
  verify, plus tamper and wrong-key rejection.
- **`decrypt` subcommand + clap-complete shell completions** — landed.
  `encrypt-payload decrypt --key-file KEY [BLOB]` accepts either a bare
  `v1.<base64>` blob as a positional, the full YAML alert body on stdin,
  or a `--file PATH`. Tolerates TradingView `{{placeholder}}` shells by
  scanning lines for `payload:` rather than parsing as YAML, so a body
  pasted straight from the alert template still decrypts. Plus
  `encrypt-payload completions <shell>` prints a clap-generated
  completion script — install with
  `encrypt-payload completions zsh > ~/.zfunc/_encrypt-payload`. 5 new
  tests for the payload extractor; round-trip with a minted blob
  verified.
- **Prep `clears` list to fix stale-ordering bug** — landed. `Intent`
  gains a `clears: Vec<String>` field. The `Prep` handler clears each
  listed prep step before recording the new one; the `Veto` handler
  does the same for vetos (symmetry). Fixes the bug where a stale
  `retest` from before `break-and-close` was satisfied stuck around
  forever and falsely satisfied future ordered gates. CLI gains
  `--clears foo,bar` on the `prep` and `veto` subcommands; the encrypt
  flow prompts for `clears` on prep/veto actions. Recent prep / veto
  names are fuzzy-picked from history (typo-proof) for both the
  `step`/`name` field and the list-of-names prompts. 84 core tests +
  55 cli tests pass; clippy clean.
- **YAML wire format + interactive template-driven encoder** — landed. Interactive prompts wired through `dialoguer` (gated behind the `cli` feature).
- **Queryable state endpoint + CLI state-management client** — landed.
  `status` and `unlock` actions go through the same encrypted envelope as
  enter/close/invalidate. KV maintains `index:cooldowns` and `index:seen`
  JSON arrays alongside the TTL keys so `status` can list them. CLI gains
  `status` and `unlock <INSTRUMENT>` subcommands that POST to the deployed
  worker via reqwest::blocking. 68 lib tests pass.

## Phase 2 ideas (parked — captured for later, not building yet)

### Multi-stage trendline workflow

Instead of one single alert fires-and-enters, a setup is built up by a *chain* of TradingView alerts, each advancing a state machine inside the webhook:

1. **Break-and-close alert** — TradingView fires when price breaks and closes through a hand-drawn trendline. Worker records `setup:<id>` in pending state.
2. **Retest alert** — fires when price retraces back to the trendline. Worker advances the setup to "armed".
3. **Entry-candle alert** — fires on the next candle's signal. Worker only places the order if the setup id is `armed` and not invalidated.
4. **Pre-fill SL-hit alert** — a separate alert at the planned stop-loss price. If this fires before the entry-candle alert, the setup id is locked out for 12h (and any pending order cancelled).

Implications for the encrypted intent format:
- Add a `setup_id` field separate from the per-message `id` — the chain shares one `setup_id`, each alert has a unique `id` for replay protection.
- Add an `expected_state` field per alert: `expect: break_close | retest | entry | invalidate_at_sl`. Worker transitions the state machine instead of blindly placing an order.
- `StateStore` needs a `get_setup_state(setup_id)` / `set_setup_state(setup_id, state, ttl)` pair on top of the current `seen` / `cooldown` pair.

This is significantly more state than what the MVP carries; design it after the simple pin-bar flow is proven in live use.

When this lands, the CLI gains a third subcommand `list-setups` — show all setup state machines and their current state. Reuses the existing `status` / `unlock` plumbing.

### Carried-over blocker

`cargo build --target wasm32-unknown-unknown --lib` currently fails inside
`oanda-client` with a `BidAskDataSource: Send` regression — pre-existing,
not introduced by anything in this repo. Needs an upstream fix in
oanda-client before `wrangler deploy` will work again.
