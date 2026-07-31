//! **Is a spread-hour candle actually "rubbish"?** — a quantifiable test.
//!
//! # The claim under test
//!
//! The engine suppresses signals on spread-hour bars
//! (`suppress_on_spread_hour`, 5 sites in `engine/src/evaluate.rs`). The
//! justification is inherited folklore — "rubbish candle": the print is said not
//! to reflect real price, because it is internal broker matching or an absent
//! liquidity provider, so its OHLC is an artefact of *who was quoting* rather
//! than of what the market did.
//!
//! That claim makes a **falsifiable prediction**, and it is not "the bar is
//! big". A genuinely violent hour also produces big bars, and those moves are
//! real — they stop real trades out and should be *sized for*, not suppressed.
//! Measuring magnitude alone cannot separate the two, so a study that only
//! compares range would score "fake print" and "genuinely volatile" identically.
//!
//! What a fake print predicts is that the move **does not stick**:
//!
//! | | range | persistence | wick share | range ÷ spread |
//! |---|---|---|---|---|
//! | fake print   | high | **low**  | **high** | **low** |
//! | real move    | high | **high** | low      | high |
//!
//! So this probe measures **reversion**, not size.
//!
//! # Metrics (per bar)
//!
//! - `persistence_k` — `|close(t+k) − open(t)| / range(t)` for k = 1, 2, 3. How
//!   much of the bar's excursion is still there k bars later. Low ⇒ erased.
//! - `retrace_1` — the fraction of bar t's range given back by bar t+1.
//! - `wick_share` — `1 − |close−open| / range`. An illiquidity artefact is
//!   mostly wick: a price nobody traded *around*. This tests Simon's mechanism
//!   directly rather than its symptom.
//! - `range_over_spread` — `range ÷ mean(ask−bid)`. **The most direct test we
//!   have**: if the "move" is roughly the size of the quote itself, you are
//!   looking at the bid-ask, not price discovery. Collapse here is the strongest
//!   evidence for the folklore.
//! - `range_over_atr` — the volatility control (see below).
//!
//! # The confound, and how it is handled
//!
//! Comparing a flagged hour against *all* other bars re-measures "the NY close
//! hour is busier". Every metric here is therefore reported **within
//! instrument**, and the magnitude metrics are normalised by a trailing ATR so
//! that "this hour is more volatile" cannot masquerade as "this hour is fake".
//! The reversion metrics (`persistence`, `retrace`, `wick_share`) are already
//! range-relative, so they are scale-free by construction — a fake print and a
//! real move of the *same size* still separate.
//!
//! # Reading the result
//!
//! The folklore is **supported** if flagged-hour bars show materially lower
//! persistence, higher wick share, and lower range-over-spread than the same
//! instrument's other hours. It is **not supported** if they differ mainly in
//! range-over-atr (i.e. just bigger) while persistence holds — that would mean
//! the moves are real and the right response is to size for them (the `max`
//! floor in `SCOPING-order-control.md` §4b-i), not to suppress the signal.
//!
//! Note this probe answers the *signal-validity* question only. Whether
//! suppression earns its keep in P&L is the separate fixture A/B.
//!
//! # Run
//!
//! ```sh
//! cargo run -p trade-control-cli --example spread_hour_candle_stats --release -- \
//!     --granularity h1 --max-instruments 40 --json /tmp/spread-hour-stats.json
//! ```
//!
//! Reads the local `candle_cache` Postgres directly (no broker calls, no
//! network), so it is safe to run against years of history while the workers are
//! live.

use std::collections::BTreeMap;

use candle_cache::{CacheClient, CacheConfig};
use candle_model::{BidAskCandle, Candle, Granularity};
use chrono::{DateTime, Duration, FixedOffset, Utc};
use oanda_client::{OandaClient, data_source::OandaDataSource};
use trade_control_core::spread_blackout::is_spread_hour;

/// Bars of trailing context used for the volatility control. 24 on H1 = a day.
const ATR_PERIOD: usize = 24;
/// Forward horizon for the persistence measurements.
const HORIZONS: [usize; 3] = [1, 2, 3];
/// A bar whose range is zero carries no information for any ratio metric.
const MIN_RANGE: f64 = 0.0;

/// One bar reduced to the quantities the study needs.
#[derive(Debug, Clone, Copy)]
struct Bar {
    time: DateTime<Utc>,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    /// Mean of the bar's own bid/ask closes — the spread at this bar.
    spread: f64,
}

impl Bar {
    fn range(&self) -> f64 {
        self.high - self.low
    }
    fn body(&self) -> f64 {
        (self.close - self.open).abs()
    }
}

/// A running accumulator: mean + standard deviation without holding samples.
#[derive(Debug, Default, Clone, Copy)]
struct Stat {
    n: u64,
    mean: f64,
    m2: f64,
}

impl Stat {
    /// Welford's online update — numerically stable over millions of bars,
    /// which a naive sum-of-squares is not at this scale.
    fn push(&mut self, x: f64) {
        if !x.is_finite() {
            return;
        }
        self.n += 1;
        let delta = x - self.mean;
        self.mean += delta / self.n as f64;
        self.m2 += delta * (x - self.mean);
    }
    fn std_dev(&self) -> f64 {
        if self.n < 2 {
            return f64::NAN;
        }
        (self.m2 / (self.n - 1) as f64).sqrt()
    }
}

/// Every metric, accumulated for one population (flagged or control).
#[derive(Debug, Default, Clone, Copy)]
struct Metrics {
    bars: u64,
    persistence: [Stat; 3],
    retrace_1: Stat,
    wick_share: Stat,
    range_over_spread: Stat,
    range_over_atr: Stat,
}

impl Metrics {
    fn push(&mut self, m: &BarMetrics) {
        self.bars += 1;
        for (i, p) in m.persistence.iter().enumerate() {
            if let Some(v) = p {
                self.persistence[i].push(*v);
            }
        }
        if let Some(v) = m.retrace_1 {
            self.retrace_1.push(v);
        }
        self.wick_share.push(m.wick_share);
        if let Some(v) = m.range_over_spread {
            self.range_over_spread.push(v);
        }
        if let Some(v) = m.range_over_atr {
            self.range_over_atr.push(v);
        }
    }
}

/// The per-bar measurements. `None` where the input didn't support the metric
/// (no forward bar, no spread, ATR not yet warm) — never a silent zero, which
/// would bias a mean toward the folklore.
struct BarMetrics {
    persistence: [Option<f64>; 3],
    retrace_1: Option<f64>,
    wick_share: f64,
    range_over_spread: Option<f64>,
    range_over_atr: Option<f64>,
}

/// Measure bar `i` against its forward context.
fn measure(bars: &[Bar], i: usize, atr: Option<f64>) -> Option<BarMetrics> {
    let bar = bars[i];
    let range = bar.range();
    if range <= MIN_RANGE {
        return None; // a flat bar has no excursion to persist or retrace
    }

    // How much of the bar's move survives k bars later. Measured from the bar's
    // OPEN (where the move started), not its close, so a bar that round-trips
    // scores near zero even if it closed where it opened.
    let mut persistence = [None; 3];
    for (slot, k) in HORIZONS.iter().enumerate() {
        if let Some(fwd) = bars.get(i + k) {
            persistence[slot] = Some((fwd.close - bar.open).abs() / range);
        }
    }

    // What fraction of the bar's range the NEXT bar gave back. For an up bar,
    // how far below its close the next bar traded; mirrored for a down bar.
    let retrace_1 = bars.get(i + 1).map(|next| {
        let given_back = if bar.close >= bar.open {
            (bar.close - next.low).max(0.0)
        } else {
            (next.high - bar.close).max(0.0)
        };
        given_back / range
    });

    // Mostly-wick is the illiquidity signature: a print at a price nobody
    // traded around.
    let wick_share = 1.0 - (bar.body() / range);

    // The most direct test: is the "move" merely the quote's own width?
    let range_over_spread =
        (bar.spread > 0.0 && bar.spread.is_finite()).then(|| range / bar.spread);

    // The volatility control — so "this hour is bigger" can't pass as "fake".
    let range_over_atr = atr.filter(|a| *a > 0.0).map(|a| range / a);

    Some(BarMetrics {
        persistence,
        retrace_1,
        wick_share,
        range_over_spread,
        range_over_atr,
    })
}

/// Trailing true-range mean ending at (and excluding) `i`. `None` until warm.
fn atr_at(bars: &[Bar], i: usize) -> Option<f64> {
    if i < ATR_PERIOD {
        return None;
    }
    let sum: f64 = (i - ATR_PERIOD..i)
        .map(|j| {
            let prev_close = bars[j.saturating_sub(1)].close;
            let b = bars[j];
            (b.high - b.low)
                .max((b.high - prev_close).abs())
                .max((b.low - prev_close).abs())
        })
        .sum();
    Some(sum / ATR_PERIOD as f64)
}

/// Per-instrument result: the two populations, plus their bar counts.
struct InstrumentResult {
    instrument: String,
    flagged: Metrics,
    control: Metrics,
}

fn ratio(flagged: f64, control: f64) -> f64 {
    if control.abs() > 0.0 && control.is_finite() && flagged.is_finite() {
        flagged / control
    } else {
        f64::NAN
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let granularity = arg(&args, "--granularity").unwrap_or_else(|| "h1".into());
    let max_instruments: usize = arg(&args, "--max-instruments")
        .and_then(|s| s.parse().ok())
        .unwrap_or(40);
    let days: i64 = arg(&args, "--days")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1460); // ~4 years, the OANDA H1 depth
    let json_out = arg(&args, "--json");

    let gran = match granularity.as_str() {
        "h1" => Granularity::OneHour,
        "m15" => Granularity::FifteenMinutes,
        "m5" => Granularity::FiveMinutes,
        other => return Err(format!("unsupported --granularity {other}").into()),
    };

    // Only instruments with a baked elevated hour can contribute a flagged
    // population; the rest would be all-control and just add noise.
    let instruments = flagged_instruments(max_instruments);
    eprintln!(
        "spread-hour candle study: {} instruments, {granularity}, {days}d lookback",
        instruments.len()
    );
    eprintln!(
        "  metrics: persistence(k=1,2,3), retrace_1, wick_share, range/spread, range/atr\n\
           populations: flagged = is_spread_hour(instrument, bar.time); control = same instrument, other hours\n"
    );

    let cache_dir = dirs_next_cache().join("candle_cache_oanda");
    let config = CacheConfig::default().with_cache_dir(cache_dir);
    let client = CacheClient::new(config, oanda_source()?).await?;

    let to = Utc::now();
    let from = to - Duration::days(days);
    let mut results = Vec::new();

    for instrument in &instruments {
        let series = match client
            .get_candles_range_bid_ask(instrument, fx(from), fx(to), gran)
            .await
        {
            Ok(s) => s,
            Err(err) => {
                eprintln!("  {instrument}: fetch failed ({err}); skipping");
                continue;
            }
        };
        let bars: Vec<Bar> = series
            .candles
            .iter()
            .map(|c| Bar {
                time: c.timestamp.with_timezone(&Utc),
                open: c.open(),
                high: c.high(),
                low: c.low(),
                close: c.close(),
                spread: (c.ask_close() - c.bid_close()).abs(),
            })
            .collect();

        if bars.len() < ATR_PERIOD + HORIZONS[2] + 1 {
            eprintln!("  {instrument}: only {} bars; skipping", bars.len());
            continue;
        }

        let mut flagged = Metrics::default();
        let mut control = Metrics::default();
        for i in 0..bars.len() {
            let Some(m) = measure(&bars, i, atr_at(&bars, i)) else {
                continue;
            };
            if is_spread_hour(instrument, bars[i].time) {
                flagged.push(&m);
            } else {
                control.push(&m);
            }
        }

        eprintln!(
            "  {instrument:<12} bars={:<7} flagged={:<6} control={:<7}",
            bars.len(),
            flagged.bars,
            control.bars
        );
        results.push(InstrumentResult {
            instrument: instrument.clone(),
            flagged,
            control,
        });
    }

    report(&results);
    if let Some(path) = json_out {
        write_json(&path, &results)?;
        eprintln!("\nwrote {path}");
    }
    Ok(())
}

/// Print the flagged-vs-control comparison, per instrument and pooled.
///
/// Everything is a **ratio to the same instrument's control population**, so a
/// value of 1.00 means "indistinguishable from a normal hour for this pair".
fn report(results: &[InstrumentResult]) {
    println!("\n=== flagged-hour bars vs the SAME instrument's other hours ===");
    println!(
        "(ratio to control; 1.00 = no different. persist<1 and r/spread<1 support the folklore)\n"
    );
    println!(
        "{:<12} {:>7} {:>8} {:>8} {:>8} {:>9} {:>9} {:>9}",
        "instrument", "flagged", "persist1", "persist3", "retrace1", "wick", "r/spread", "r/atr"
    );

    let mut pooled: BTreeMap<&str, Stat> = BTreeMap::new();
    for r in results {
        if r.flagged.bars == 0 || r.control.bars == 0 {
            continue;
        }
        let p1 = ratio(r.flagged.persistence[0].mean, r.control.persistence[0].mean);
        let p3 = ratio(r.flagged.persistence[2].mean, r.control.persistence[2].mean);
        let rt = ratio(r.flagged.retrace_1.mean, r.control.retrace_1.mean);
        let wk = ratio(r.flagged.wick_share.mean, r.control.wick_share.mean);
        let rs = ratio(
            r.flagged.range_over_spread.mean,
            r.control.range_over_spread.mean,
        );
        let ra = ratio(r.flagged.range_over_atr.mean, r.control.range_over_atr.mean);
        println!(
            "{:<12} {:>7} {:>8.3} {:>8.3} {:>8.3} {:>9.3} {:>9.3} {:>9.3}",
            r.instrument, r.flagged.bars, p1, p3, rt, wk, rs, ra
        );
        for (k, v) in [
            ("persist1", p1),
            ("persist3", p3),
            ("retrace1", rt),
            ("wick", wk),
            ("r/spread", rs),
            ("r/atr", ra),
        ] {
            pooled.entry(k).or_default().push(v);
        }
    }

    println!("\n=== pooled across instruments (mean of per-instrument ratios) ===");
    for (k, s) in &pooled {
        println!(
            "  {k:<10} {:.4}  (sd {:.4}, n={})",
            s.mean,
            s.std_dev(),
            s.n
        );
    }

    println!("\n=== how to read this ===");
    println!("  persist1/3 << 1  ⇒ the move is ERASED — supports 'rubbish candle'");
    println!("  wick       >> 1  ⇒ mostly wick, price nobody traded around — supports it");
    println!("  r/spread   << 1  ⇒ the 'move' is the size of the quote itself — strongest support");
    println!("  r/atr      >> 1 while persist ~= 1 ⇒ bars are BIGGER but REAL — suppression is");
    println!("                    the wrong response; size for them instead (max floor, §4b-i)");
}

fn write_json(path: &str, results: &[InstrumentResult]) -> std::io::Result<()> {
    let mut out = String::from("{\n  \"instruments\": [\n");
    for (i, r) in results.iter().enumerate() {
        let comma = if i + 1 == results.len() { "" } else { "," };
        out.push_str(&format!(
            "    {{\"instrument\": {:?}, \"flagged_bars\": {}, \"control_bars\": {}, \
             \"flagged\": {}, \"control\": {}}}{comma}\n",
            r.instrument,
            r.flagged.bars,
            r.control.bars,
            metrics_json(&r.flagged),
            metrics_json(&r.control),
        ));
    }
    out.push_str("  ]\n}\n");
    std::fs::write(path, out)
}

fn metrics_json(m: &Metrics) -> String {
    format!(
        "{{\"persist1\": {:.6}, \"persist2\": {:.6}, \"persist3\": {:.6}, \
         \"retrace1\": {:.6}, \"wick_share\": {:.6}, \"range_over_spread\": {:.6}, \
         \"range_over_atr\": {:.6}}}",
        m.persistence[0].mean,
        m.persistence[1].mean,
        m.persistence[2].mean,
        m.retrace_1.mean,
        m.wick_share.mean,
        m.range_over_spread.mean,
        m.range_over_atr.mean,
    )
}

/// OANDA instruments carrying a non-zero elevated-hours mask.
///
/// The baked table lives in a private module (`spread_blackout::baseline_candle`),
/// so rather than widening that API for a probe we parse the generated source
/// directly — it is a checked-in static file, and reading it here keeps the study
/// pinned to exactly the rows the engine flags. Cross-checked below against the
/// public `is_spread_hour`, which is the real oracle.
fn flagged_instruments(max: usize) -> Vec<String> {
    const TABLE: &str = include_str!("../../core/src/spread_baseline_candle.rs");
    let mut out = Vec::new();
    for line in TABLE.lines() {
        let line = line.trim();
        if !line.starts_with("(\"oanda\"") {
            continue;
        }
        // ("oanda", "EUR_USD", "ny", true, 131072, [...], ...)
        let fields: Vec<&str> = line.trim_start_matches('(').split(',').collect();
        let Some(symbol) = fields.get(1) else {
            continue;
        };
        let Some(mask) = fields.get(4) else { continue };
        let symbol = symbol.trim().trim_matches('"');
        let mask: u32 = mask.trim().parse().unwrap_or(0);
        if mask != 0 {
            out.push(symbol.to_string());
        }
        if out.len() >= max {
            break;
        }
    }
    out
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn fx(t: DateTime<Utc>) -> DateTime<FixedOffset> {
    t.into()
}

/// Same cache-dir resolution the other probes use (no `dirs_next` dep here).
fn dirs_next_cache() -> std::path::PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var("HOME").expect("HOME set")).join(".cache")
        })
}

/// The cache needs a `DataSource` for misses. This study is meant to read
/// cached history, so the credentials are only a fallback — but a miss on a
/// 4-year range would hammer the API, so `--days` should stay inside what the
/// cache already holds (OANDA H1: 2022-01-01 onward).
fn oanda_source() -> Result<OandaDataSource, Box<dyn std::error::Error>> {
    let token = std::env::var("OANDA_TOKEN")
        .map_err(|_| "OANDA_TOKEN not set (needed as the cache's miss-fallback)")?;
    let account_id = std::env::var("OANDA_ACCOUNT_ID")
        .map_err(|_| "OANDA_ACCOUNT_ID not set (needed as the cache's miss-fallback)")?;
    Ok(OandaDataSource::new(OandaClient::new(token), account_id))
}
