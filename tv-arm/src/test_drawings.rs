//! Chart-drawing constructors shared by the resolver test suites.
//!
//! Every resolver test starts from the same place: a handful of synthetic
//! `Drawing`s standing in for what the operator drew on the chart. Those
//! constructors were duplicated inside `pipeline`'s test module while the
//! H&S, M/W, and plan-emission suites all lived there together; splitting
//! those suites into their own modules would have meant copying the
//! constructors three times, and a fixture that drifts between copies is a
//! test suite that quietly stops testing the same thing.
//!
//! So they live here, `pub(crate)` and compiled only under `cfg(test)`.
//!
//! The shapes here are deliberately *realistic* rather than minimal — see
//! [`fib`] in particular, whose point order mirrors real TradingView readback
//! and is the reason a point-order bug was catchable at all.

use chrono::{DateTime, Utc};
use trading_view::drawings::{Drawing, Point, Properties};

/// The fixed "now" every test clock is anchored to.
///
/// Tests that build a future `trade_expiry` do it relative to this, so the
/// suite doesn't start failing the day the real wall clock passes a
/// hard-coded literal.
pub(crate) fn now() -> DateTime<Utc> {
    "2026-06-08T00:00:00Z"
        .parse()
        .expect("the fixed test clock is a valid RFC3339 instant")
}

/// A representative arm-time spread (1 pip) the pure resolvers bake.
///
/// The live read that produces this in `run` is exercised separately (the
/// `spread` module's own tests + the demo protocol), not here.
pub(crate) const SPREAD: f64 = 1.0;

/// A single-anchor vertical line at `unix` — a trade-expiry or news marker.
pub(crate) fn vline(id: &str, unix: i64) -> Drawing {
    Drawing {
        id: id.to_string(),
        points: vec![Point {
            time: unix,
            price: 1.0,
        }],
        properties: Properties {
            text: None,
            ..Default::default()
        },
    }
}

/// A two-anchor line from `a` to `b` (e.g. a neckline trend line), in draw
/// order, optionally labelled.
///
/// For a *fib* use [`fib`] instead — the head↔neckline mapping there is
/// `reverse`-dependent, not raw point order.
///
/// An empty `label` means *no* text property at all, which is a different
/// thing from `Some("")` to the role matcher.
pub(crate) fn two_point(id: &str, label: &str, a: f64, b: f64) -> Drawing {
    Drawing {
        id: id.to_string(),
        points: vec![Point { time: 10, price: a }, Point { time: 20, price: b }],
        properties: Properties {
            text: (!label.is_empty()).then(|| label.to_string()),
            ..Default::default()
        },
    }
}

/// A fib retracement whose `(head, neckline)` resolve as given, built to
/// match real TradingView readback: with `reverse: false` the `0`-reading
/// (head) sits at `points[1]` and the `1`-level (neckline) at `points[0]`.
/// This deliberately mirrors the AUD/CAD 2026-07 shape where the head is
/// `points[1]` — the exact case that broke the point-order rule.
pub(crate) fn fib(id: &str, head: f64, neckline: f64) -> Drawing {
    Drawing {
        id: id.to_string(),
        // points[0] = neckline (1-level), points[1] = head (0-level).
        points: vec![
            Point {
                time: 20,
                price: neckline,
            },
            Point {
                time: 10,
                price: head,
            },
        ],
        properties: Properties {
            reverse: Some(false),
            ..Default::default()
        },
    }
}

/// A single-anchor horizontal line at `price` with `label`.
pub(crate) fn hline(id: &str, label: &str, price: f64) -> Drawing {
    Drawing {
        id: id.to_string(),
        points: vec![Point { time: 15, price }],
        properties: Properties {
            text: Some(label.to_string()),
            ..Default::default()
        },
    }
}

/// An M/W path with exactly three anchors.
pub(crate) fn path(id: &str, prices: [f64; 3]) -> Drawing {
    path_n(id, &prices)
}

/// An M/W path with exactly four anchors (the right-shoulder form).
pub(crate) fn path4(id: &str, prices: [f64; 4]) -> Drawing {
    path_n(id, &prices)
}

/// An M/W path with an arbitrary anchor count.
///
/// Anchor *count* is load-bearing — three and four are the only legal
/// shapes, and both too-few and too-many must be rejected rather than
/// silently truncated — so the arity-generic form is the one the rejection
/// tests drive.
pub(crate) fn path_n(id: &str, prices: &[f64]) -> Drawing {
    Drawing {
        id: id.to_string(),
        points: prices
            .iter()
            .enumerate()
            .map(|(i, &p)| Point {
                time: (i as i64 + 1) * 10,
                price: p,
            })
            .collect(),
        properties: Properties {
            text: None,
            ..Default::default()
        },
    }
}
