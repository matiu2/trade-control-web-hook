//! Building the signed **control bundles** — one `pause` bundle per blackout
//! window, one `news` bundle per news window.
//!
//! Extracted from `pipeline.rs` unchanged. The two flavours differ only in their
//! spec/built types and their output subdirectory, so the build loop, the
//! guards, and the `trade_id` requirement live once behind [`ControlKind`]
//! rather than as two hand-copied loops — the guards are the interesting part (an
//! empty window set is fine; a non-empty set with no `trade_id` is a refusal),
//! and a copy-pasted pair drifts silently: one gets a fix, the other doesn't, and
//! the divergence shows up only as a half-armed trade.
//!
//! One thing to preserve if you touch the directory naming: pause and news
//! bundles **must** land in disjoint directories. Both write a `manifest.yaml`,
//! so a shared directory silently clobbers one of them.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use color_eyre::eyre::{Context, Result, eyre};
use tracing::info;
use trade_control_cli as cli;
use trade_control_conventions::Broker;
use trade_control_core::sig::KEY_LEN;

use crate::broker_kind::broker_to_kind;
use crate::news_window::NewsWindow;

/// In-memory representation of one built pause / news bundle so the
/// payload loop downstream can iterate without re-reading disk.
pub struct Bundle<K: ControlKind> {
    pub built: K::Built,
    out_dir: PathBuf,
}

/// The two flavours of control bundle differ only in their spec/built types and
/// their output subdirectory prefix — the build loop, the guards, and the
/// `trade_id` requirement are identical. This trait is that difference, so the
/// loop exists once.
///
/// Worth doing rather than two hand-copied loops: the guards are the interesting
/// part (an empty window set is fine; a non-empty set with no `trade_id` is a
/// refusal), and a copy-paste pair drifts silently — one gets a fix, the other
/// doesn't, and the divergence only shows up as a half-armed trade.
pub trait ControlKind {
    /// The signed bundle this kind produces.
    type Built;
    /// Subdirectory prefix under the arm's out-dir: `pause-1`, `news-2`, …
    const DIR_PREFIX: &'static str;
    /// Human name for the error/log text.
    const NAME: &'static str;

    /// Build and sign one bundle for `window`, writing it into `dir`.
    fn build(
        ctx: &BundleContext<'_>,
        window: &NewsWindow,
        dir: &Path,
        idx: usize,
    ) -> Result<Self::Built>;
}

pub struct PauseKind;
pub struct NewsKind;

impl ControlKind for PauseKind {
    type Built = cli::BuiltPause;
    const DIR_PREFIX: &'static str = "pause";
    const NAME: &'static str = "blackout";

    fn build(
        ctx: &BundleContext<'_>,
        window: &NewsWindow,
        dir: &Path,
        idx: usize,
    ) -> Result<Self::Built> {
        let spec = cli::PauseSpec {
            trade_id: ctx.trade_id.to_string(),
            blackout_id: None,
            instrument: ctx.instrument.to_string(),
            account: ctx.account.to_string(),
            broker: broker_to_kind(ctx.broker),
            start_time: window.start(),
            end_time: window.end(),
            reason: Some(ctx.reason(window)),
        };
        let built = cli::build_pause_from_spec(spec, ctx.now)
            .with_context(|| format!("build pause #{idx}"))?;
        cli::write_pause(&built, ctx.key, dir).with_context(|| format!("write pause #{idx}"))?;
        Ok(built)
    }
}

impl ControlKind for NewsKind {
    type Built = cli::BuiltNews;
    const DIR_PREFIX: &'static str = "news";
    const NAME: &'static str = "news";

    fn build(
        ctx: &BundleContext<'_>,
        window: &NewsWindow,
        dir: &Path,
        idx: usize,
    ) -> Result<Self::Built> {
        let spec = cli::NewsSpec {
            trade_id: ctx.trade_id.to_string(),
            news_id: None,
            instrument: ctx.instrument.to_string(),
            account: ctx.account.to_string(),
            broker: broker_to_kind(ctx.broker),
            start_time: window.start(),
            end_time: window.end(),
            reason: Some(ctx.reason(window)),
        };
        let built = cli::build_news_from_spec(spec, ctx.now)
            .with_context(|| format!("build news #{idx}"))?;
        cli::write_news(&built, ctx.key, dir).with_context(|| format!("write news #{idx}"))?;
        Ok(built)
    }
}

/// Everything a control bundle needs beyond the window itself. Grouped because
/// the two builders previously took eight positional arguments each — enough
/// that two same-typed `&str`s (`instrument`, `account`) could be transposed at
/// a call site with no compiler complaint.
pub struct BundleContext<'a> {
    pub trade_id: &'a str,
    pub instrument: &'a str,
    pub account: &'a str,
    pub broker: Broker,
    pub out_dir: &'a Path,
    pub key: &'a [u8; KEY_LEN],
    /// The as-of instant the bundle is built against — the same prune yardstick
    /// the windows survived, so a window kept as upcoming-vs-cursor isn't then
    /// rejected as stale by the builder's own past-window guard.
    pub now: DateTime<Utc>,
}

impl<'a> BundleContext<'a> {
    /// The `reason` string stamped on a control alert, identifying the trade and
    /// the window it came from.
    fn reason(&self, window: &NewsWindow) -> String {
        format!("news:{}-{}", self.instrument, window.start().to_rfc3339())
    }

    /// Build one bundle per window, writing each into its own numbered
    /// subdirectory.
    ///
    /// An empty window set is not an error — most trades have no news in their
    /// lifetime. A *non-empty* set with no `trade_id`, though, is a refusal:
    /// control alerts are trade-scoped, so an unscoped one would pause the whole
    /// instrument rather than this trade.
    pub fn build_all<K: ControlKind>(&self, windows: &[NewsWindow]) -> Result<Vec<Bundle<K>>> {
        if windows.is_empty() {
            return Ok(Vec::new());
        }
        if self.trade_id.is_empty() {
            return Err(eyre!(
                "have {} windows but trade has no trade_id; refusing to arm",
                K::NAME
            ));
        }
        let bundles = windows
            .iter()
            .enumerate()
            .map(|(i, window)| {
                let idx = i + 1;
                let dir = self.out_dir.join(format!("{}-{idx}", K::DIR_PREFIX));
                fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
                Ok(Bundle {
                    built: K::build(self, window, &dir, idx)?,
                    out_dir: dir,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        info!(
            kind = K::NAME,
            count = bundles.len(),
            "control bundles built"
        );
        Ok(bundles)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::news_window::NewsWindow;

    fn now() -> DateTime<Utc> {
        "2026-06-08T00:00:00Z".parse().expect("fixed timestamp")
    }

    fn nw(start_unix: i64, end_unix: i64) -> NewsWindow {
        NewsWindow::new(
            DateTime::<Utc>::from_timestamp(start_unix, 0).expect("valid start"),
            DateTime::<Utc>::from_timestamp(end_unix, 0).expect("valid end"),
        )
    }

    /// Pause and news bundles must land in **disjoint** directories.
    ///
    /// Both `write_pause` and `write_news` emit a `manifest.yaml` into the
    /// directory they're handed. If the two kinds ever shared a numbered
    /// subdirectory, the second writer would silently overwrite the first's
    /// manifest — a half-armed trade whose on-disk record is wrong, with no
    /// error anywhere.
    ///
    /// This is the one thing that genuinely distinguishes the two `ControlKind`
    /// impls now that the build loop is shared, so it's asserted rather than
    /// assumed: swapping `DIR_PREFIX` on either impl must fail here.
    #[test]
    fn pause_and_news_bundles_never_share_a_directory() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let key = [7u8; KEY_LEN];
        let t = now().timestamp();
        let ctx = BundleContext {
            trade_id: "t-abc123",
            instrument: "EUR_USD",
            account: "ms-oanda-1",
            broker: Broker::Oanda,
            out_dir: tmp.path(),
            key: &key,
            now: now(),
        };
        // Two of each, so the numbering (`-1`, `-2`) is exercised too — a
        // shared counter across kinds would also collide.
        let windows = vec![nw(t + 600, t + 1200), nw(t + 1800, t + 2400)];

        let pauses = match ctx.build_all::<PauseKind>(&windows) {
            Ok(b) => b,
            Err(e) => panic!("pause bundles must build: {e}"),
        };
        let newses = match ctx.build_all::<NewsKind>(&windows) {
            Ok(b) => b,
            Err(e) => panic!("news bundles must build: {e}"),
        };
        assert_eq!(pauses.len(), 2);
        assert_eq!(newses.len(), 2);
        // Numbering is 1-based and per-kind. Nothing downstream parses it, but
        // it's what the operator reads in the arm directory, so pin the spelling
        // rather than let it drift silently.
        fn dir_name<K: ControlKind>(b: &Bundle<K>) -> &str {
            b.out_dir
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("<none>")
        }
        assert_eq!(dir_name(&pauses[0]), "pause-1");
        assert_eq!(dir_name(&pauses[1]), "pause-2");
        assert_eq!(dir_name(&newses[0]), "news-1");

        let dirs: Vec<&Path> = pauses
            .iter()
            .map(|b| b.out_dir.as_path())
            .chain(newses.iter().map(|b| b.out_dir.as_path()))
            .collect();
        let unique: std::collections::HashSet<&Path> = dirs.iter().copied().collect();
        assert_eq!(
            unique.len(),
            4,
            "four bundles need four distinct directories, got {dirs:?}"
        );

        // …and each really wrote its own manifest, i.e. nothing was clobbered.
        for dir in &dirs {
            assert!(
                dir.join("manifest.yaml").is_file(),
                "{} has no manifest",
                dir.display()
            );
        }
        // The pause dirs hold pause specs and the news dirs hold news specs —
        // proves the collision check above isn't passing on name alone.
        for b in &pauses {
            assert!(b.out_dir.join("pause.yaml").is_file());
        }
        for b in &newses {
            assert!(b.out_dir.join("news.yaml").is_file());
        }
    }

    /// An empty window set is normal (most trades have no news in their
    /// lifetime) and must not error — whereas a non-empty set with no
    /// `trade_id` must refuse, since an unscoped control alert would pause the
    /// whole instrument instead of this one trade.
    #[test]
    fn build_all_allows_no_windows_but_refuses_windows_without_a_trade_id() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let key = [7u8; KEY_LEN];
        let t = now().timestamp();
        let mut ctx = BundleContext {
            trade_id: "",
            instrument: "EUR_USD",
            account: "ms-oanda-1",
            broker: Broker::Oanda,
            out_dir: tmp.path(),
            key: &key,
            now: now(),
        };

        match ctx.build_all::<PauseKind>(&[]) {
            Ok(b) => assert!(b.is_empty(), "no windows → no bundles"),
            Err(e) => panic!("no windows must not error: {e}"),
        }

        match ctx.build_all::<PauseKind>(&[nw(t + 600, t + 1200)]) {
            Ok(_) => panic!("windows with no trade_id must refuse"),
            Err(e) => assert!(
                e.to_string().contains("no trade_id"),
                "unhelpful error: {e}"
            ),
        }

        // With a trade_id it builds.
        ctx.trade_id = "t-abc123";
        match ctx.build_all::<PauseKind>(&[nw(t + 600, t + 1200)]) {
            Ok(b) => assert_eq!(b.len(), 1),
            Err(e) => panic!("should build with a trade_id: {e}"),
        }
    }
}
