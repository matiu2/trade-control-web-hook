//! `journal` — a keyboard-first TUI to walk old `trade-control` plans, load
//! them into TradingView, replay them, and delete once journalled.
//!
//! Environment-suffixed like `trade-control` / `tv-arm`: `journal-staging`
//! drives `trade-control-staging` + `replay-candles-staging` (see `build.rs`).
//!
//! Navigation is a left→right screen stack: List → Timeline → Replay → Compare.
//! `→`/`n` push deeper, `←` pop back to the list. See `screen.rs`.

mod app;
mod cli;
mod divergence;
mod jobs;
mod keys;
mod plan;
mod record;
mod screen;
mod timeline;
mod tv;
mod ui;

use std::io::{Stdout, stdout};
use std::time::Duration;

use clap::Parser;
use color_eyre::eyre::Result;
use crossterm::event::{self, Event};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::app::App;

/// The baked git version (see `build.rs`).
const VERSION: &str = env!("GIT_VERSION");

#[derive(Parser)]
#[command(name = "journal", version = VERSION, about = "Journal old trade-control plans")]
struct Args {
    /// Fetch + parse the plan list and print it to stderr, then exit — no TUI.
    /// A smoke test for the CLI wiring and parsers.
    #[arg(long)]
    dump: bool,
}

fn main() -> Result<()> {
    color_eyre::install()?;
    init_tracing();

    let args = Args::parse();
    if args.dump {
        return dump_plans();
    }

    let mut terminal = setup_terminal()?;
    let result = run(&mut terminal);
    restore_terminal(&mut terminal)?;
    result
}

/// The main render/input loop. Background jobs (replay, timeline) run on their
/// own threads and post results to `app.drain_jobs`; the short poll timeout is
/// the redraw tick that animates the "loading…" spinner and picks up results.
fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    let mut app = App::new()?;
    while !app.should_quit {
        // A refresh (Ctrl-L) clears the back buffer so the next draw repaints
        // every cell — recovers from any residual corruption on the screen.
        if app.needs_clear {
            terminal.clear()?;
            app.needs_clear = false;
        }
        terminal.draw(|f| ui::render(f, &app))?;
        // A short poll keeps the spinner animating and lets finished jobs land
        // promptly even when the operator isn't pressing keys.
        if event::poll(Duration::from_millis(120))?
            && let Event::Key(key) = event::read()?
            && key.kind == event::KeyEventKind::Press
        {
            let action = keys::map_key(&app, key);
            keys::apply(&mut app, action);
        }
        // Apply any background results (no-op when nothing finished).
        app.drain_jobs();
        // Advance the spinner clock every pass.
        app.tick = app.tick.wrapping_add(1);
    }
    Ok(())
}

// -- terminal lifecycle ----------------------------------------------------

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    let terminal = Terminal::new(CrosstermBackend::new(out))?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

// -- dump smoke test -------------------------------------------------------

/// Fetch `plan list`, parse it, and print a compact table to stderr.
fn dump_plans() -> Result<()> {
    let yaml = cli::plan_list_yaml()?;
    let rows = plan::parse_plan_list(&yaml)?;
    eprintln!("{} plan(s):", rows.len());
    for r in &rows {
        eprintln!(
            "  {:32} {:16} {:6} {:24} {}",
            r.trade_id,
            r.instrument,
            r.granularity,
            r.phase.as_deref().unwrap_or("-"),
            if r.is_archived() { "ARCHIVED" } else { "" },
        );
    }
    Ok(())
}

/// Standard tracing init with an env-filter and the error layer. Writes to a
/// file so log lines never corrupt the alternate-screen TUI.
fn init_tracing() {
    use tracing_error::ErrorLayer;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    // Log to stderr only in --dump; under the TUI stderr is the alternate
    // screen. Keep it simple: env-filter defaults to warn so the TUI stays clean.
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(ErrorLayer::default())
        .init();
}
