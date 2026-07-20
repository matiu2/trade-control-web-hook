//! `market-hours-gen` — fetch H1 candles from OANDA + TradeNation, measure
//! ATR-relative close→open gaps, and render the committed
//! `core/src/market_hours_baked.rs` table.
//!
//! # Run
//!
//! ```sh
//! OANDA_TOKEN=... cargo run -p market-hours-gen --release -- \
//!   --out ../core/src/market_hours_baked.rs
//! ```
//!
//! Needs a practice OANDA token (`OANDA_TOKEN` or `OANDA_API_KEY`) and the
//! shared TradeNation demo session (resolved by `login_demo`). Prints a
//! validation report (which instruments got a daily-close block, at what hours)
//! then writes the table.

use std::path::PathBuf;

use clap::Parser;
use color_eyre::eyre::eyre;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, fmt};

use market_hours_gen::compute::{DAILY_TURN_ON_FRACTION, MIN_SAMPLES, profile_from_bars};
use market_hours_gen::fetch::{oanda_bars, tn_bars};
use market_hours_gen::universe::{WorkItem, work_items};
use market_hours_gen::{MarketHoursRow, Venue, render_table};

#[derive(Parser, Debug)]
#[command(about = "Generate the candle-derived market-hours blackout table")]
struct Args {
    /// Where to write the generated table (default: core/src/market_hours_baked.rs).
    #[arg(long, default_value = "../core/src/market_hours_baked.rs")]
    out: PathBuf,

    /// Include stock instruments (part-time exchanges, gappy candles). Off by
    /// default.
    #[arg(long)]
    include_stocks: bool,

    /// Only profile these venues (repeatable). Default: both.
    #[arg(long, value_parser = ["oanda", "tradenation"])]
    venue: Vec<String>,
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(filter)
        .with(tracing_error::ErrorLayer::default())
        .init();
}

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install().ok();
    init_tracing();
    let args = Args::parse();

    let venues = resolve_venues(&args.venue);
    let assets = instrument_lookup::all().map_err(|e| eyre!("instrument-lookup catalog: {e}"))?;
    let items = work_items(assets, &venues, args.include_stocks);
    tracing::info!("profiling {} (venue, symbol) work items", items.len());

    // Shared clients: one OANDA practice client, one TN demo session.
    let oanda = if venues.contains(&Venue::Oanda) {
        let token = std::env::var("OANDA_TOKEN")
            .or_else(|_| std::env::var("OANDA_API_KEY"))
            .map_err(|_| eyre!("set OANDA_TOKEN (practice) to profile OANDA"))?;
        Some(oanda_client::OandaClient::new(token))
    } else {
        None
    };
    let tn = if venues.contains(&Venue::TradeNation) {
        let session = tradenation_api::login_demo()
            .await
            .map_err(|e| eyre!("tn demo login: {e}"))?;
        Some((reqwest::Client::new(), session))
    } else {
        None
    };

    let total = items.len();
    let mut rows: Vec<MarketHoursRow> = Vec::with_capacity(total);
    for (n, item) in items.iter().enumerate() {
        tracing::info!(
            "[{}/{total}] {} {} ({})",
            n + 1,
            item.venue.as_str(),
            item.symbol,
            item.display_name
        );
        rows.push(profile_one(item, oanda.as_ref(), tn.as_ref()).await);
    }

    print_report(&rows);

    let table = render_table(&rows);
    std::fs::write(&args.out, &table)?;
    tracing::info!("wrote {} rows to {}", rows.len(), args.out.display());
    Ok(())
}

/// Resolve the `--venue` args (empty ⇒ both) into a de-duplicated list.
fn resolve_venues(names: &[String]) -> Vec<Venue> {
    if names.is_empty() {
        return vec![Venue::Oanda, Venue::TradeNation];
    }
    let mut v = Vec::new();
    for n in names {
        let venue = match n.as_str() {
            "oanda" => Venue::Oanda,
            _ => Venue::TradeNation,
        };
        if !v.contains(&venue) {
            v.push(venue);
        }
    }
    v
}

/// Fetch + profile one instrument; on fetch error emit a weekend-only row
/// flagged with the error rather than aborting the whole run.
async fn profile_one(
    item: &WorkItem,
    oanda: Option<&oanda_client::OandaClient>,
    tn: Option<&(reqwest::Client, tradenation_api::Session)>,
) -> MarketHoursRow {
    let bars = match item.venue {
        Venue::Oanda => match oanda {
            Some(c) => oanda_bars(c, &item.symbol).await,
            None => Err(eyre!("no oanda client")),
        },
        Venue::TradeNation => match tn {
            Some((http, session)) => tn_bars(http, session, &item.symbol).await,
            None => Err(eyre!("no tn session")),
        },
    };

    match bars {
        Ok(bars) => {
            let profile = profile_from_bars(&bars);
            MarketHoursRow {
                venue: item.venue,
                symbol: item.symbol.clone(),
                display_name: item.display_name.clone(),
                profile,
                error: None,
            }
        }
        Err(e) => {
            tracing::warn!("{} {}: {e}", item.venue.as_str(), item.symbol);
            MarketHoursRow {
                venue: item.venue,
                symbol: item.symbol.clone(),
                display_name: item.display_name.clone(),
                profile: profile_from_bars(&[]),
                error: Some(e.to_string()),
            }
        }
    }
}

/// Print the daily-close decisions, sorted by mid-week fraction descending, so
/// the operator can eyeball which instruments got a mid-week block and where.
fn print_report(rows: &[MarketHoursRow]) {
    println!(
        "\n===== MARKET-HOURS GEN — {} rows (weekend always-on; daily-close if \
         midweek attention >= {:.0}%, >= {MIN_SAMPLES} samples) =====\n",
        rows.len(),
        DAILY_TURN_ON_FRACTION * 100.0
    );
    let mut order: Vec<&MarketHoursRow> = rows.iter().collect();
    order.sort_by(|a, b| {
        b.profile
            .midweek_fraction
            .partial_cmp(&a.profile.midweek_fraction)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for r in order {
        if let Some(e) = &r.error {
            println!("[err ] {:12} {:12} {e}", r.venue.as_str(), r.symbol);
            continue;
        }
        if r.profile.midweek_fraction < 0.05 && !r.profile.daily_close {
            continue; // quiet: weekend-only, nothing mid-week
        }
        let flag = if r.profile.daily_close {
            "DAILY"
        } else {
            "  .  "
        };
        println!(
            "[{flag}] {:12} {:12} midweek {:5.1}%  ({}/{} gaps)  hours:{:?}",
            r.venue.as_str(),
            r.symbol,
            r.profile.midweek_fraction * 100.0,
            r.profile.midweek_attention_gaps,
            r.profile.total_gaps,
            r.profile.daily_close_hours,
        );
    }
}
