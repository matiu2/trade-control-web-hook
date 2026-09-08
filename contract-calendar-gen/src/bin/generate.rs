//! `contract-calendar-gen` — derive futures contract close-out deadlines and
//! render the committed `core/src/contract_calendar_baked.rs` table.
//!
//! # Run
//!
//! ```sh
//! cargo run -p contract-calendar-gen --release -- \
//!   --out ../core/src/contract_calendar_baked.rs
//! ```
//!
//! No network access and no credentials — every date is derived from published
//! exchange rules. Prints a validation report (each contract month with its
//! last trade day, First Notice Day and both close-out deadlines) then writes
//! the table.
//!
//! Re-run when the holiday table gains a year, or when a contract root is
//! added to `rules::CONTRACT_SPECS`.

use std::path::PathBuf;

use clap::Parser;
use color_eyre::eyre::{Context, eyre};
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, fmt};

use contract_calendar_gen::holiday::{FIRST_COVERED_YEAR, LAST_COVERED_YEAR};
use contract_calendar_gen::rules::{CLOSE_OUT_BUSINESS_DAYS, all_contract_dates};
use contract_calendar_gen::{contract_month, render_table};

#[derive(Parser, Debug)]
#[command(about = "Generate the futures contract close-out calendar table")]
struct Args {
    /// Where to write the generated table. Relative paths resolve against the
    /// workspace root (cargo's cwd for `cargo run`), not this crate's dir.
    #[arg(long, default_value = "core/src/contract_calendar_baked.rs")]
    out: PathBuf,

    /// First contract year to emit (default: the holiday table's first year).
    #[arg(long)]
    from_year: Option<i32>,

    /// Last contract year to emit (default: the holiday table's last year).
    #[arg(long)]
    to_year: Option<i32>,

    /// Print the report without writing the table.
    #[arg(long)]
    dry_run: bool,
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(filter)
        .with(tracing_error::ErrorLayer::default())
        .init();
}

fn main() -> color_eyre::Result<()> {
    color_eyre::install().ok();
    init_tracing();
    let args = Args::parse();

    let from = args.from_year.unwrap_or(FIRST_COVERED_YEAR);
    let to = args.to_year.unwrap_or(LAST_COVERED_YEAR);
    if from > to {
        return Err(eyre!("--from-year {from} is after --to-year {to}"));
    }
    // Refuse rather than emit rows whose business-day arithmetic would run off
    // the end of the holiday table — a deadline computed from weekends alone
    // lands LATER than the truth, which is the unsafe direction.
    if from < FIRST_COVERED_YEAR || to > LAST_COVERED_YEAR {
        return Err(eyre!(
            "requested {from}..={to} but the holiday table only covers \
             {FIRST_COVERED_YEAR}..={LAST_COVERED_YEAR} — extend `holiday::HOLIDAYS` first"
        ));
    }

    let years: Vec<i32> = (from..=to).collect();
    let rows = all_contract_dates(&years);
    if rows.is_empty() {
        return Err(eyre!("no contract months derived for {from}..={to}"));
    }

    report(&rows);

    let table = render_table(&rows).map_err(|e| eyre!("{e}"))?;
    if args.dry_run {
        tracing::info!("--dry-run: not writing {}", args.out.display());
        return Ok(());
    }
    // Refuse rather than create a stray tree: a mistyped --out that silently
    // wrote somewhere unexpected would leave `core` including a stale table.
    let parent = args
        .out
        .parent()
        .ok_or_else(|| eyre!("--out {} has no parent directory", args.out.display()))?;
    if !parent.as_os_str().is_empty() && !parent.is_dir() {
        return Err(eyre!(
            "--out directory {} does not exist (run from the workspace root, \
             or pass an explicit path)",
            parent.display()
        ));
    }
    std::fs::write(&args.out, table).wrap_err_with(|| format!("writing {}", args.out.display()))?;
    tracing::info!(
        rows = rows.len(),
        out = %args.out.display(),
        "wrote contract calendar"
    );
    Ok(())
}

/// Print the human validation report — the operator reads this to sanity-check
/// that gold's long deadline really does land a month before expiry.
fn report(rows: &[contract_calendar_gen::ContractDates]) {
    println!(
        "\nFutures close-out calendar ({CLOSE_OUT_BUSINESS_DAYS} business days before \
         the reference day)\n"
    );
    println!(
        "  {:<5} {:<8} {:<10} {:<12} {:<12} {:<12}",
        "ROOT", "MONTH", "LAST_TRADE", "FIRST_NOTICE", "LONG_BY", "SHORT_BY"
    );
    for r in rows {
        let fnd = r
            .first_notice_day
            .map(|d| d.to_string())
            .unwrap_or_else(|| "-".to_string());
        println!(
            "  {:<5} {:<8} {:<10} {:<12} {:<12} {:<12}",
            r.root,
            contract_month(r.year, r.month),
            r.last_trade_day,
            fnd,
            r.long_close_out,
            r.short_close_out,
        );
    }
    println!("\n  {} contract months\n", rows.len());
}
