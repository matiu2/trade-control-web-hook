#!/usr/bin/env python3
"""Measure pre-entry context (trend, RSI, MFI divergence) against the fixture corpus.

For every fixture cell that placed an entry, this reconstructs what the
indicators looked like on the bars **before** the entry and asks whether any of
them separates the winners from the losers.

Approximations, stated up front -- these are Python re-implementations, not the
shipped Rust indicators:

* **RSI** mirrors `indicator-rsi`: Wilder smoothing, seeded with the SMA of the
  first `period` changes, on **mid** closes (the `Candle` trait convention --
  indicators use mid, only entries/exits use bid/ask).

* **MFI is volume-weightless.** The shipped `mfi` crate multiplies typical price
  by `candle.volume()`, and the corpus candles carry **no volume field at all**
  (fields are time/o/h/l/c + bid_*/ask_*). So a faithful MFI is not computable
  here. What this computes is the money-flow ratio with volume held at 1 --
  i.e. a typical-price RSI. It is reported as `mfi_nv` and must NOT be read as
  the real MFI; it is directional-only. Feed real volume to get the true value.

* **Trend** is measured three ways so one definition can't drive the result:
  EMA(fast) vs EMA(slow), the slope of a linear fit over a lookback, and the
  position of price within its lookback range.

* **Divergence** compares the price extreme against the indicator's value at
  two swing points inside the lookback: price makes a higher high while the
  indicator makes a lower high (bearish), or the mirror (bullish).

The output is a per-bucket win rate and mean R. Nothing here changes the corpus.
"""

from __future__ import annotations

import argparse
import json
import math
import statistics
import sys
from collections import defaultdict
from pathlib import Path

COLUMNS = ["normal", "skip-bcr", "strategy-v2", "strategy-v2-qm-market"]


# --------------------------------------------------------------------------
# Indicators
# --------------------------------------------------------------------------
def rsi_series(closes, period=14):
    """Wilder RSI, matching indicator-rsi/src/calculator.rs.

    Returns a list aligned to `closes`, with None until warmup completes
    (`period + 1` candles, i.e. `period` changes).
    """
    out = [None] * len(closes)
    if len(closes) < period + 1:
        return out
    gains, losses = [], []
    avg_gain = avg_loss = None
    for i in range(1, len(closes)):
        change = closes[i] - closes[i - 1]
        gain, loss = max(change, 0.0), max(-change, 0.0)
        if avg_gain is None:
            gains.append(gain)
            losses.append(loss)
            if len(gains) < period:
                continue
            avg_gain = sum(gains) / period
            avg_loss = sum(losses) / period
        else:
            avg_gain = (avg_gain * (period - 1) + gain) / period
            avg_loss = (avg_loss * (period - 1) + loss) / period
        out[i] = 100.0 if avg_loss < 1e-12 else 100.0 - 100.0 / (1.0 + avg_gain / avg_loss)
    return out


def mfi_no_volume_series(candles, period=14):
    """Money-flow index with volume held at 1.0 -- NOT the real MFI.

    The corpus has no volume, so raw money flow degenerates to typical price.
    Mirrors the mfi crate's structure (typical price, up/down classification,
    rolling window) so the shape is comparable, but the volume weighting that
    makes MFI distinct from RSI is absent. Reported as `mfi_nv`.
    """
    out = [None] * len(candles)
    tps = [(c["h"] + c["l"] + c["c"]) / 3.0 for c in candles]
    window = []
    for i in range(1, len(candles)):
        window.append((tps[i], tps[i] > tps[i - 1]))
        if len(window) > period:
            window.pop(0)
        if len(window) == period:
            pos = sum(f for f, up in window if up)
            neg = sum(f for f, up in window if not up)
            out[i] = 100.0 if neg < 1e-12 else 100.0 - 100.0 / (1.0 + pos / neg)
    return out


def ema_series(values, period):
    out = [None] * len(values)
    if len(values) < period:
        return out
    k = 2.0 / (period + 1.0)
    ema = sum(values[:period]) / period
    out[period - 1] = ema
    for i in range(period, len(values)):
        ema = values[i] * k + ema * (1 - k)
        out[i] = ema
    return out


def slope_pct_per_bar(values):
    """Least-squares slope over `values`, normalised to % of mean price per bar.

    Normalising makes the number comparable across instruments -- a raw slope on
    JPY pairs and on EUR/USD are not the same unit.
    """
    n = len(values)
    if n < 2:
        return None
    mean_x = (n - 1) / 2.0
    mean_y = sum(values) / n
    denom = sum((i - mean_x) ** 2 for i in range(n))
    if denom < 1e-12 or abs(mean_y) < 1e-12:
        return None
    slope = sum((i - mean_x) * (v - mean_y) for i, v in enumerate(values)) / denom
    return slope / mean_y * 100.0


def find_swings(values, left=2, right=2, kind="high"):
    """Indices of local extremes with `left`/`right` bars strictly beyond.

    A fractal-style pivot. Used to anchor divergence on actual swing points
    rather than on the arbitrary endpoints of the lookback window.
    """
    idx = []
    for i in range(left, len(values) - right):
        v = values[i]
        if v is None:
            continue
        window = values[i - left : i + right + 1]
        if any(w is None for w in window):
            continue
        if kind == "high" and all(v >= w for w in window) and any(v > w for w in window):
            idx.append(i)
        if kind == "low" and all(v <= w for w in window) and any(v < w for w in window):
            idx.append(i)
    return idx


def divergence(prices, indicator, lo, hi, direction):
    """Detect regular divergence between price and an indicator in [lo, hi).

    direction 'short' looks for bearish (price higher high, indicator lower
    high); 'long' looks for bullish (price lower low, indicator higher low).
    Returns True/False, or None when there aren't two usable swings.
    """
    kind = "high" if direction == "short" else "low"
    seg_p = prices[lo:hi]
    seg_i = indicator[lo:hi]
    swings = find_swings(seg_p, kind=kind)
    swings = [s for s in swings if seg_i[s] is not None]
    if len(swings) < 2:
        return None
    a, b = swings[-2], swings[-1]
    if kind == "high":
        return seg_p[b] > seg_p[a] and seg_i[b] < seg_i[a]
    return seg_p[b] < seg_p[a] and seg_i[b] > seg_i[a]


# --------------------------------------------------------------------------
# Corpus
# --------------------------------------------------------------------------
def parse_cell(name):
    if "-sl-" in name:
        return None
    for news in ("on", "off"):
        tail = f"-news-{news}"
        if name.endswith(tail):
            rest = name[: -len(tail)]
            break
    else:
        return None
    for col in sorted(COLUMNS, key=len, reverse=True):
        if rest.endswith(f"-{col}"):
            return rest[: -len(col) - 1], col, news
    return None


def entry_context(cell_dir, lookback, rsi_period, ema_fast, ema_slow):
    """Indicator state on the bar BEFORE the first entry of this cell.

    Returns None when the cell never traded or the candle history is too short
    to warm the indicators -- both are reported, never silently treated as zero.
    """
    exp = json.loads((cell_dir / "expected.json").read_text())
    legs = (exp.get("outcome") or {}).get("legs") or []
    if not legs:
        return None
    leg = legs[0]
    candles = json.loads((cell_dir / "candles.json").read_text())
    times = [c["time"] for c in candles]
    try:
        ei = times.index(leg["entry_time"])
    except ValueError:
        return None
    # Strictly before the entry bar: no lookahead.
    if ei < max(lookback, rsi_period + 2, ema_slow + 1):
        return None

    closes = [c["c"] for c in candles]
    highs = [c["h"] for c in candles]
    lows = [c["l"] for c in candles]

    rsi = rsi_series(closes, rsi_period)
    mfi = mfi_no_volume_series(candles, rsi_period)
    ef = ema_series(closes, ema_fast)
    es = ema_series(closes, ema_slow)

    p = ei - 1  # last fully-formed bar before entry
    if rsi[p] is None or ef[p] is None or es[p] is None:
        return None

    direction = "short" if leg["take_profit"] < leg["entry_price"] else "long"
    seg = closes[ei - lookback : ei]
    win_hi, win_lo = max(highs[ei - lookback : ei]), min(lows[ei - lookback : ei])
    rng = win_hi - win_lo

    return {
        "r": leg["r"],
        "direction": direction,
        "rsi": rsi[p],
        "mfi_nv": mfi[p],
        "ema_gap_pct": (ef[p] - es[p]) / es[p] * 100.0,
        "slope_pct": slope_pct_per_bar(seg),
        "pos_in_range": (closes[p] - win_lo) / rng if rng > 1e-12 else None,
        "rsi_div": divergence(highs if direction == "short" else lows, rsi,
                              ei - lookback, ei, direction),
        "mfi_div": divergence(highs if direction == "short" else lows, mfi,
                              ei - lookback, ei, direction),
    }


def bucket_report(rows, key, edges, label):
    """Win rate + mean R per bucket of a continuous feature."""
    vals = [r for r in rows if r.get(key) is not None]
    if not vals:
        print(f"\n{label}: no usable values")
        return
    print(f"\n{label}  (n={len(vals)})")
    print(f"  {'bucket':<22} {'n':>4} {'win%':>6} {'meanR':>8} {'totalR':>8}")
    buckets = defaultdict(list)
    for r in vals:
        v = r[key]
        name = f"< {edges[0]:g}"
        for i, e in enumerate(edges):
            if v >= e:
                name = f">= {e:g}" if i == len(edges) - 1 else f"{e:g}..{edges[i+1]:g}"
        buckets[name].append(r["r"])
    order = [f"< {edges[0]:g}"] + [
        (f">= {e:g}" if i == len(edges) - 1 else f"{e:g}..{edges[i+1]:g}")
        for i, e in enumerate(edges)
    ]
    for name in order:
        rs = buckets.get(name)
        if not rs:
            continue
        win = sum(1 for x in rs if x > 0) / len(rs) * 100
        print(f"  {name:<22} {len(rs):>4} {win:>5.0f}% {statistics.fmean(rs):>+8.2f} {sum(rs):>+8.2f}")


def flag_report(rows, key, label):
    """Win rate + mean R split on a boolean feature (None = undetectable)."""
    groups = defaultdict(list)
    for r in rows:
        groups[r.get(key)].append(r["r"])
    print(f"\n{label}")
    print(f"  {'value':<22} {'n':>4} {'win%':>6} {'meanR':>8} {'totalR':>8}")
    for val in (True, False, None):
        rs = groups.get(val)
        if not rs:
            continue
        name = {True: "divergence", False: "none", None: "not detectable"}[val]
        win = sum(1 for x in rs if x > 0) / len(rs) * 100
        print(f"  {name:<22} {len(rs):>4} {win:>5.0f}% {statistics.fmean(rs):>+8.2f} {sum(rs):>+8.2f}")


def main():
    root = Path(__file__).resolve().parent.parent / "replay-fixtures"
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--fixtures-dir", type=Path, default=root)
    ap.add_argument("--column", default="skip-bcr", choices=COLUMNS + ["all"])
    ap.add_argument("--news", default="on", choices=["on", "off"])
    ap.add_argument("--lookback", type=int, default=30)
    ap.add_argument("--rsi-period", type=int, default=14)
    ap.add_argument("--ema-fast", type=int, default=20)
    ap.add_argument("--ema-slow", type=int, default=50)
    ap.add_argument("--dump-csv", type=Path)
    args = ap.parse_args()

    rows, skipped = [], 0
    for d in sorted(args.fixtures_dir.glob("*/")):
        coord = parse_cell(d.name)
        if not coord:
            continue
        setup, col, news = coord
        if news != args.news:
            continue
        if args.column != "all" and col != args.column:
            continue
        try:
            ctx = entry_context(d, args.lookback, args.rsi_period, args.ema_fast, args.ema_slow)
        except (OSError, json.JSONDecodeError, KeyError):
            skipped += 1
            continue
        if ctx is None:
            skipped += 1
            continue
        ctx["setup"], ctx["column"] = setup, col
        rows.append(ctx)

    print(f"entries analysed: {len(rows)}   (skipped {skipped}: no trade, short history, or unreadable)")
    if not rows:
        return 1
    allr = [r["r"] for r in rows]
    print(f"baseline: win {sum(1 for x in allr if x>0)/len(allr)*100:.0f}%  "
          f"meanR {statistics.fmean(allr):+.2f}  totalR {sum(allr):+.2f}")
    print(f"direction mix: {sum(1 for r in rows if r['direction']=='short')} short / "
          f"{sum(1 for r in rows if r['direction']=='long')} long")

    bucket_report(rows, "rsi", [30, 45, 55, 70], "RSI on the bar before entry")
    bucket_report(rows, "mfi_nv", [30, 45, 55, 70], "MFI (VOLUME-LESS approximation) before entry")
    bucket_report(rows, "ema_gap_pct", [-0.5, -0.1, 0.1, 0.5], f"Trend: EMA{args.ema_fast}-EMA{args.ema_slow} gap %")
    bucket_report(rows, "slope_pct", [-0.1, -0.02, 0.02, 0.1], f"Trend: slope %/bar over {args.lookback} bars")
    bucket_report(rows, "pos_in_range", [0.25, 0.5, 0.75], f"Position in {args.lookback}-bar range")
    flag_report(rows, "rsi_div", "RSI divergence (regular, in trade direction)")
    flag_report(rows, "mfi_div", "MFI-no-volume divergence (regular, in trade direction)")

    if args.dump_csv:
        import csv
        cols = ["setup", "column", "direction", "r", "rsi", "mfi_nv", "ema_gap_pct",
                "slope_pct", "pos_in_range", "rsi_div", "mfi_div"]
        with args.dump_csv.open("w", newline="") as fh:
            w = csv.DictWriter(fh, fieldnames=cols)
            w.writeheader()
            for r in rows:
                w.writerow({c: r.get(c) for c in cols})
        print(f"\nwrote {args.dump_csv}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
