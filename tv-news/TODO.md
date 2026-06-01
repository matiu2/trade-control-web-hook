# tv-news — sentiment port

Port of trade-calendar-maker's `sentiment` feature into tv-news. Same
algorithm, adapted to consume the asset's `news_currencies` (from
instrument-lookup) rather than trade-calendar-maker's `Instrument`.

## Plan

- [x] Add `sentiment/parser.rs` (verbatim copy — pure value parser, no
      deps on the rest of trade-calendar-maker).
- [x] Add `sentiment/rules.rs` (verbatim copy — event-name rule lookup
      and per-event direction).
- [x] Add `sentiment.rs` adapted to take `news_currencies: &[String]`
      rather than `Instrument`. Same lookback (24h, or back to Friday on
      Monday). Same per-currency scoring.
- [x] Wire into `pipeline::run` after the filter phase: run on the
      already-fetched events, log a one-line per-currency summary plus
      the overall direction.
- [x] `--no-sentiment` flag to disable (default on).
- [x] Tests pass; cargo clippy clean; cargo fmt applied.
- [x] README updated.
- [x] Commit + push.
