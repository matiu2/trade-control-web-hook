# TODO: arm datetime + sentiment journalling — ✅ DONE

**Status: all four PRs landed on branch `feature/arm-datetime-and-sentiment`.**
Full workspace green (tests + clippy + fmt). Summary:

- PR-1 `427155d` — extracted tv-news sentiment → shared serializable
  `news-sentiment-tv` crate.
- PR-2 `8e71f22` — `TradePlan.armed_at`; records `--start` cursor when
  journaling, else `Utc::now()`. Surfaces on `plan show` verbatim.
- PR-3 `92d5712` — `TradePlan.armed_sentiment` (lean `core::PlanSentiment`);
  tv-arm computes + prints + bakes it as of the effective arm time. Fail-soft.
- PR-4 `78c9b65` — replay-candles recomputes sentiment for its window (as of
  `armed_at`/window start) and prints it. **Not** added to the golden
  `ReplayOutcome` JSON (kept deterministic) — see note below.

**Deviation from the original plan:** request 2 said "text + JSON
`ReplayOutcome`". The JSON was deliberately *not* touched — `ReplayOutcome`
is a golden fixture compared for byte-equality, and a live forex-factory
fetch would make it non-deterministic. Sentiment is a human-report
annotation only.

---

Three related features, all journalling/read-back aids (no worker behaviour change):

1. **`armed_at` on the plan** — bake the tv-arm run datetime onto `TradePlan`
   so it can be read back later. Like `replay_start`: rides the whole-plan
   signed line, `#[serde(default, skip_serializing_if)]`, engine ignores it.

2. **News sentiment in replay-candles output** — recompute the same
   sentiment tv-news produces, for the replay window, and show it in the
   report (text + JSON `ReplayOutcome`).

3. **Print sentiment when tv-arm runs + record it in the plan** — compute
   at arm time, print it, and bake a `SentimentSnapshot` onto `TradePlan`
   for after-the-fact journalling. Engine ignores it.

Requests 2 & 3 both need the tv-news sentiment logic shared + serializable.

## Plan (one small change per commit, tests first)

### PR-1: Extract sentiment into a shared, serializable lib crate
- [ ] New crate `news-sentiment-tv` (workspace member) — move
      `tv-news/src/sentiment.rs` + `sentiment/{rules,parser}.rs` in.
- [ ] Add `Serialize`/`Deserialize` to `SentimentAnalysis`,
      `CurrencySentiment`, `Confidence`, `SentimentDirection`, `EventSentiment`.
- [ ] `tv-news` depends on the new crate; delete its copy; keep behaviour
      byte-identical (log output unchanged).
- [ ] Tests: move existing sentiment tests; add a serde round-trip test.
- [ ] clippy + fmt, commit.

### PR-2: `armed_at` on TradePlan
- [ ] Add `armed_at: Option<DateTime<Utc>>` to `TradePlan`
      (`#[serde(default, skip_serializing_if = "Option::is_none")]`).
- [ ] Thread `now` into `build_trade_plan`; set `armed_at: Some(now)`.
- [ ] Update the test helper that names every field.
- [ ] Surface in `plan show` / relevant read-back path.
- [ ] Tests: plan with armed_at round-trips; pre-field plan deserializes as None.
- [ ] clippy + fmt, commit.

### PR-3: Sentiment snapshot on TradePlan + print at arm time
- [ ] Add `armed_sentiment: Option<SentimentSnapshot>` to `TradePlan`
      (signed-line, serde default+skip). `SentimentSnapshot` = the serializable
      `SentimentAnalysis` (or a trimmed journalling view).
- [ ] tv-arm: at arm time, fetch events + `analyze_sentiment`, print it,
      bake onto the plan.
- [ ] Fail-soft: a sentiment fetch failure must NOT block arming (warn + None).
- [ ] Tests.
- [ ] clippy + fmt, commit.

### PR-4: replay-candles sentiment output
- [ ] replay-candles fetches forex-factory events for its window, computes
      sentiment, prints in the text report, adds to JSON `ReplayOutcome`.
- [ ] Fail-soft.
- [ ] Tests.
- [ ] clippy + fmt, commit.

## Notes / hazards
- Signed body is top-level lines only; anything nested in `TradePlan` rides
  one `trade_plan:` flow line → no HMAC fingerprint change, pre-field plans
  round-trip. (Do NOT add a new top-level `Intent` field for these.)
- `EconomicEvent`/`Impact` already derive serde (forex_factory git dep).
- `DateTime<Local>` in the sentiment structs serializes with offset — fine.
- CLAUDE.md: 2024 edition, no mod.rs, no unwrap/expect outside tests,
  tracing, color_eyre, `cargo add`/`remove` not manual Cargo.toml edits.
