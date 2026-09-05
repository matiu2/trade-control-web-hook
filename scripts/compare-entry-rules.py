#!/usr/bin/env python3
"""Compare entry-rule columns across the replay-fixtures corpus.

Reads the blessed `expected.json` of every fixture cell and ranks the four
entry-rule columns by realised R. No replay is run -- the corpus is already
blessed, so this is a read.

Two corpus hazards are handled explicitly rather than averaged over:

1. **Unequal SL-anchor coverage.** Only 26 of 68 setups have `-sl-fib-top` /
   `-sl-invalidation` variants. Pooling every cell would weight those 26
   setups 3x. The headline holds the anchor fixed at the default (`signal`);
   `--sl-anchor` re-runs the whole comparison on another anchor.

2. **A mostly-dead news axis.** Every `news-off` cell captured before
   2026-08-15 is a byte-duplicate of its `news-on` twin (the flag was consumed
   upstream of SetupInputs). Only ~16 of 243 pairs actually differ. Averaging
   both would double-count each setup, so one side is chosen (`--news`).

Comparison is **paired**: only setups where every requested column is present
count toward the headline, so no column is flattered by sitting out the
setups that hurt it. Unpaired setups are reported separately, never silently
dropped.
"""

from __future__ import annotations

import argparse
import json
import statistics
import sys
from collections import defaultdict
from pathlib import Path

# Ordered worst->best assumed prior; only affects tie-break display order.
COLUMNS = ["normal", "skip-bcr", "strategy-v2", "strategy-v2-qm-market"]

# The corpus label for each column, spelled as the operator would ask for it.
COLUMN_ALIASES = {
    "strategy-v2": "strategy-v2 (--qm-entry limit, the default QM leg)",
    "strategy-v2-qm-market": "strategy-v2 (--qm-entry market)",
    "skip-bcr": "skip-bcr (no break-and-close required)",
    "normal": "normal (baseline H&S)",
}

SL_SUFFIX = {"signal": "", "fib-top": "-sl-fib-top", "invalidation": "-sl-invalidation"}


class Cell:
    """One fixture directory: a (setup, column, news, sl-anchor) coordinate."""

    __slots__ = ("name", "setup", "column", "news", "sl", "net_r", "legs", "phase")

    def __init__(self, name, setup, column, news, sl, net_r, legs, phase):
        self.name = name
        self.setup = setup
        self.column = column
        self.news = news
        self.sl = sl
        self.net_r = net_r
        self.legs = legs
        self.phase = phase

    @property
    def traded(self) -> bool:
        """Did this cell actually place an entry? A 0.0R no-trade is not a
        breakeven trade, and conflating them is how a column that simply never
        fires looks 'safe'."""
        return self.legs > 0


def parse_cell_name(name: str):
    """Split a fixture dir name into (setup, column, news, sl-anchor).

    Returns None for cells outside the grid (hand-made regression fixtures like
    `coffee-sad` or `xau-xag-tp-resistance`), which carry no column coordinate
    and must not be scored as if they did.
    """
    rest = name
    sl = "signal"
    for anchor, suffix in SL_SUFFIX.items():
        if suffix and rest.endswith(suffix):
            rest = rest[: -len(suffix)]
            sl = anchor
            break

    for news in ("on", "off"):
        tail = f"-news-{news}"
        if rest.endswith(tail):
            rest = rest[: -len(tail)]
            break
    else:
        return None

    for column in sorted(COLUMNS, key=len, reverse=True):
        tail = f"-{column}"
        if rest.endswith(tail):
            return rest[: -len(tail)], column, news, sl
    return None


def load_corpus(root: Path):
    """Read every grid cell. Non-grid and unreadable cells are returned
    separately so they are reported, not silently skipped."""
    cells, skipped = [], []
    for meta in sorted(root.glob("*/expected.json")):
        name = meta.parent.name
        coord = parse_cell_name(name)
        if coord is None:
            skipped.append((name, "not a grid cell"))
            continue
        setup, column, news, sl = coord
        try:
            data = json.loads(meta.read_text())
        except (OSError, json.JSONDecodeError) as exc:
            skipped.append((name, f"unreadable: {exc}"))
            continue
        outcome = data.get("outcome") or {}
        net_r = outcome.get("net_r")
        if net_r is None:
            skipped.append((name, "no outcome.net_r"))
            continue
        cells.append(
            Cell(
                name=name,
                setup=setup,
                column=column,
                news=news,
                sl=sl,
                net_r=float(net_r),
                legs=len(outcome.get("legs") or []),
                phase=data.get("final_phase"),
            )
        )
    return cells, skipped


def summarise(rs):
    """Descriptive stats for one column's per-setup R values."""
    traded = [r for r in rs if r is not None]
    return {
        "n": len(traded),
        "total": sum(traded),
        "mean": statistics.fmean(traded) if traded else 0.0,
        "median": statistics.median(traded) if traded else 0.0,
        "wins": sum(1 for r in traded if r > 0),
        "losses": sum(1 for r in traded if r < 0),
        "flat": sum(1 for r in traded if r == 0),
        "best": max(traded) if traded else 0.0,
        "worst": min(traded) if traded else 0.0,
    }


def build_grid(cells, news, sl):
    """setup -> column -> Cell, for one (news, sl-anchor) slice."""
    grid = defaultdict(dict)
    for c in cells:
        if c.news == news and c.sl == sl:
            grid[c.setup][c.column] = c
    return grid


def fmt_r(x):
    return f"{x:+.2f}"


def report(cells, columns, news, sl, verbose, top):
    grid = build_grid(cells, news, sl)

    paired = {s: row for s, row in grid.items() if all(c in row for c in columns)}
    unpaired = {s: row for s, row in grid.items() if s not in paired}

    print(f"\n{'=' * 78}")
    print(f"ENTRY-RULE COMPARISON   news={news}   sl-anchor={sl}")
    print(f"{'=' * 78}")
    print(f"setups with all {len(columns)} columns present : {len(paired)}")
    if unpaired:
        print(f"setups skipped (incomplete row)          : {len(unpaired)}")

    if not paired:
        print("\nNo setup has every requested column -- nothing to compare.")
        return None

    per_col = {c: [paired[s][c].net_r for s in paired] for c in columns}
    traded_col = {c: [paired[s][c] for s in paired if paired[s][c].traded] for c in columns}

    print(f"\n{'column':<44} {'total R':>9} {'mean':>7} {'med':>7} {'W/L/0':>10} {'traded':>7}")
    print("-" * 90)
    ranked = sorted(columns, key=lambda c: sum(per_col[c]), reverse=True)
    for c in ranked:
        s = summarise(per_col[c])
        label = COLUMN_ALIASES.get(c, c)
        wl = f"{s['wins']}/{s['losses']}/{s['flat']}"
        print(
            f"{label:<44} {fmt_r(s['total']):>9} {fmt_r(s['mean']):>7} "
            f"{fmt_r(s['median']):>7} {wl:>10} {len(traded_col[c]):>7}"
        )

    winner = ranked[0]
    print(f"\nBest by total R: {COLUMN_ALIASES.get(winner, winner)}")

    # Head-to-head against the winner: per-setup deltas say whether the lead is
    # broad or the artefact of one or two outliers.
    print(f"\nHead-to-head vs {winner} (per-setup delta, + = {winner} better):")
    print("-" * 90)
    for c in ranked[1:]:
        deltas = [paired[s][winner].net_r - paired[s][c].net_r for s in paired]
        nonzero = [d for d in deltas if abs(d) > 1e-9]
        better = sum(1 for d in nonzero if d > 0)
        worse = sum(1 for d in nonzero if d < 0)
        print(
            f"  vs {c:<38} sum {fmt_r(sum(deltas)):>8}  "
            f"differs on {len(nonzero):>3}/{len(deltas)} setups  "
            f"({better} better, {worse} worse)"
        )
        if nonzero and top:
            ranked_d = sorted(
                ((d, s) for d, s in zip(deltas, paired) if abs(d) > 1e-9),
                key=lambda t: abs(t[0]),
                reverse=True,
            )[:top]
            for d, s in ranked_d:
                print(f"      {fmt_r(d):>8}  {s}")

    if verbose:
        print(f"\nPer-setup R by column ({len(paired)} setups):")
        header = f"{'setup':<44}" + "".join(f"{c[:14]:>15}" for c in ranked)
        print(header)
        print("-" * len(header))
        for s in sorted(paired, key=lambda s: -paired[s][winner].net_r):
            row = f"{s:<44}"
            for c in ranked:
                cell = paired[s][c]
                mark = " " if cell.traded else "*"
                row += f"{fmt_r(cell.net_r) + mark:>15}"
            print(row)
        print("\n* = no entry placed (0.0R is a no-trade, not a breakeven)")

    if unpaired and verbose:
        print("\nIncomplete rows (missing at least one column):")
        for s in sorted(unpaired):
            have = ",".join(sorted(unpaired[s]))
            print(f"  {s:<44} has: {have}")

    return {c: sum(per_col[c]) for c in columns}


def main():
    default_root = Path(__file__).resolve().parent.parent / "replay-fixtures"
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--fixtures-dir", type=Path, default=default_root)
    ap.add_argument("--news", choices=["on", "off"], default="on",
                    help="which news column to score (default on; the news-off "
                         "cells written before 2026-08-15 are duplicates)")
    ap.add_argument("--sl-anchor", choices=sorted(SL_SUFFIX), default="signal",
                    help="hold the SL anchor fixed (default signal, the shipped default)")
    ap.add_argument("--all-slices", action="store_true",
                    help="run every (news, sl-anchor) slice as a robustness check")
    ap.add_argument("--columns", nargs="+", default=COLUMNS)
    ap.add_argument("--top", type=int, default=5, help="largest per-setup deltas to show (0 = none)")
    ap.add_argument("-v", "--verbose", action="store_true", help="per-setup table")
    args = ap.parse_args()

    if not args.fixtures_dir.is_dir():
        print(f"error: no such fixtures dir: {args.fixtures_dir}", file=sys.stderr)
        return 2

    cells, skipped = load_corpus(args.fixtures_dir)
    print(f"read {len(cells)} grid cells from {args.fixtures_dir}")
    if skipped:
        print(f"({len(skipped)} non-grid or unreadable cells ignored)")
        if args.verbose:
            for name, why in skipped:
                print(f"  - {name}: {why}")

    unknown = [c for c in args.columns if c not in COLUMNS]
    if unknown:
        print(f"error: unknown column(s): {unknown}; known: {COLUMNS}", file=sys.stderr)
        return 2

    if args.all_slices:
        tallies = {}
        for news in ("on", "off"):
            for sl in sorted(SL_SUFFIX):
                got = report(cells, args.columns, news, sl, args.verbose, args.top)
                if got:
                    tallies[(news, sl)] = got
        print(f"\n{'=' * 78}")
        print("ROBUSTNESS: winner by total R in each slice")
        print(f"{'=' * 78}")
        for (news, sl), t in tallies.items():
            win = max(t, key=t.get)
            print(f"  news={news:<4} sl={sl:<13} -> {win:<26} ({fmt_r(t[win])})")
    else:
        report(cells, args.columns, args.news, args.sl_anchor, args.verbose, args.top)
    return 0


if __name__ == "__main__":
    sys.exit(main())
