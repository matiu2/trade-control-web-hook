#!/usr/bin/env python3
"""Rough equity simulation over the blessed fixture corpus.

Takes the realised R of every leg in one entry-rule column, orders them by
**exit time**, and compounds a starting balance risking a fixed % per entry.

Deliberately rough -- stated so the numbers aren't over-read:

* **R is currency-agnostic.** A leg's `r` is already "multiples of the risked
  amount", so no FX conversion is needed or done. 1% risk means a -1.00R leg
  costs exactly 1% of the balance it was sized against.
* **No costs beyond what the replay already modelled.** Spread is in the fill
  prices; financing/commission/slippage-past-stop are not.
* **Sizing is off the balance at ENTRY time**, and positions overlap (up to 8
  concurrently in this corpus), so several legs can be sized off the same
  balance before any of them resolves. That is what live trading does. The
  `--sequential` mode instead pretends trades never overlap; it is the
  optimistic textbook curve and is offered only for contrast.
* **This is one fixed historical path**, not a distribution. 71 legs over ~11
  weeks of hand-picked charts.

Note on the bootstrap: **reshuffling the leg order is a no-op.** Sequential
fixed-fractional compounding is a product of `(1 + risk*r_i)` terms, and
multiplication commutes -- every permutation of the same legs lands on exactly
the same final balance. So the bootstrap resamples **with replacement**, which
asks the question that actually has an answer: how different could the result
have been had we drawn a different 71 trades from the same distribution?
"""

from __future__ import annotations

import argparse
import json
import random
import statistics
import sys
from datetime import datetime
from pathlib import Path

COLUMNS = ["normal", "skip-bcr", "strategy-v2", "strategy-v2-qm-market"]


def parse_ts(s):
    return datetime.fromisoformat(s.replace("Z", "+00:00"))


def load_legs(root: Path, column: str, news: str):
    """Every leg of one column, with entry/exit times. Sorted by exit."""
    legs = []
    tail = f"-{column}-news-{news}"
    for d in sorted(root.glob("*/")):
        name = d.name
        if not name.endswith(tail) or "-sl-" in name:
            continue
        try:
            exp = json.loads((d / "expected.json").read_text())
        except (OSError, json.JSONDecodeError):
            continue
        for leg in ((exp.get("outcome") or {}).get("legs") or []):
            if leg.get("r") is None:
                continue
            legs.append({
                "setup": name[: -len(tail)],
                "entry": parse_ts(leg["entry_time"]),
                "exit": parse_ts(leg["exit_time"]),
                "r": float(leg["r"]),
                "reason": leg.get("exit_reason"),
            })
    legs.sort(key=lambda l: (l["exit"], l["entry"]))
    return legs


def simulate(legs, start, risk_pct, sequential):
    """Compound `start` risking `risk_pct` of balance per entry.

    Concurrent positions are sized off the balance as it stood at their own
    entry, which is why an open-position-aware run differs from the sequential
    one. Returns the per-leg trail.
    """
    balance = start
    peak = start
    trail = []
    # Balance at each entry time, needed to size a trade opened before earlier
    # trades have closed.
    resolved = []  # (exit_time, r) already applied
    for leg in legs:
        if sequential:
            sized_off = balance
        else:
            # Balance as of this leg's ENTRY: start + every leg that had
            # already exited by then.
            sized_off = start
            for ex, r in resolved:
                if ex <= leg["entry"]:
                    sized_off *= 1 + risk_pct * r
        risk_amount = sized_off * risk_pct
        pnl = risk_amount * leg["r"]
        balance += pnl
        resolved.append((leg["exit"], leg["r"]))
        peak = max(peak, balance)
        trail.append({
            **leg,
            "risk_amount": risk_amount,
            "pnl": pnl,
            "balance": balance,
            "drawdown_pct": (balance - peak) / peak * 100 if peak else 0.0,
        })
    return trail


def sparkline(values, width=64, height=14):
    """ASCII equity curve -- no plotting library, renders in a terminal."""
    if not values:
        return []
    lo, hi = min(values), max(values)
    if hi - lo < 1e-9:
        hi = lo + 1
    step = max(1, len(values) / width)
    sampled = [values[min(len(values) - 1, int(i * step))] for i in range(width)]
    rows = []
    for h in range(height, 0, -1):
        hi_band = lo + (hi - lo) * h / height
        lo_band = lo + (hi - lo) * (h - 1) / height
        line = "".join("#" if lo_band <= v <= hi_band or (h == height and v >= hi_band)
                       else ("|" if v > hi_band else " ") for v in sampled)
        rows.append((hi_band, line))
    return rows


def report(trail, start, risk_pct, label):
    if not trail:
        print("no legs")
        return
    bal = [start] + [t["balance"] for t in trail]
    final = bal[-1]
    peak = max(bal)
    maxdd = min(t["drawdown_pct"] for t in trail)
    wins = [t for t in trail if t["r"] > 0]
    losses = [t for t in trail if t["r"] < 0]

    print(f"\n{'=' * 72}")
    print(f"{label}")
    print(f"{'=' * 72}")
    print(f"  start balance      ${start:,.2f}")
    print(f"  final balance      ${final:,.2f}")
    print(f"  profit             ${final - start:,.2f}  ({(final/start - 1)*100:+.1f}%)")
    print(f"  peak balance       ${peak:,.2f}")
    print(f"  max drawdown       {maxdd:.1f}%")
    print(f"  legs               {len(trail)}  ({len(wins)}W / {len(losses)}L / "
          f"{len(trail)-len(wins)-len(losses)} flat)")
    print(f"  win rate           {len(wins)/len(trail)*100:.0f}%")
    print(f"  total R            {sum(t['r'] for t in trail):+.2f}")
    if wins and losses:
        aw = statistics.fmean(t["r"] for t in wins)
        al = statistics.fmean(t["r"] for t in losses)
        print(f"  avg win / loss     {aw:+.2f}R / {al:+.2f}R")
    print(f"  period             {trail[0]['exit'].date()} -> {trail[-1]['exit'].date()}")

    print(f"\n  equity curve (${min(bal):,.0f} .. ${max(bal):,.0f}):")
    for hi_band, line in sparkline(bal):
        print(f"   {hi_band:>9,.0f} |{line}")
    print(f"   {'':>9} +{'-' * 64}")
    print(f"   {'':>9}  {trail[0]['exit'].date()}{' ' * 44}{trail[-1]['exit'].date()}")


def bootstrap(legs, start, risk_pct, n, seed):
    """Resample legs WITH REPLACEMENT to bound how lucky this run was.

    Deliberately not a reshuffle: sequential fixed-fractional compounding is a
    product of `(1 + risk*r)` terms, so permuting the legs cannot change the
    final balance at all (verified -- all 24 permutations of a 4-leg set give
    the same cent). Sampling with replacement instead draws a different set of
    71 trades from the same empirical distribution, which is the question worth
    asking. Still assumes the legs are independent and identically distributed,
    which hand-picked charts only roughly are.
    """
    rs = [l["r"] for l in legs]
    rng = random.Random(seed)
    finals = []
    for _ in range(n):
        b = start
        for _ in range(len(rs)):
            b += b * risk_pct * rng.choice(rs)
        finals.append(b)
    finals.sort()
    q = lambda p: finals[int(p * (len(finals) - 1))]
    print(f"\n  bootstrap ({n:,} resamples with replacement, {len(rs)} legs each):")
    print(f"    median  ${statistics.median(finals):,.0f}")
    print(f"    5%-95%  ${q(0.05):,.0f} .. ${q(0.95):,.0f}")
    print(f"    worst   ${finals[0]:,.0f}     best ${finals[-1]:,.0f}")
    print(f"    losing runs: {sum(1 for f in finals if f < start)/len(finals)*100:.0f}%")


def main():
    root = Path(__file__).resolve().parent.parent / "replay-fixtures"
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--fixtures-dir", type=Path, default=root)
    ap.add_argument("--column", default="skip-bcr", choices=COLUMNS)
    ap.add_argument("--news", default="on", choices=["on", "off"])
    ap.add_argument("--start", type=float, default=10_000.0)
    ap.add_argument("--risk-pct", type=float, default=1.0, help="percent of balance risked per entry")
    ap.add_argument("--sequential", action="store_true",
                    help="pretend trades never overlap (optimistic; real corpus has up to 8 concurrent)")
    ap.add_argument("--bootstrap", type=int, default=10_000)
    ap.add_argument("--seed", type=int, default=0)
    ap.add_argument("--csv", type=Path)
    ap.add_argument("-v", "--verbose", action="store_true", help="per-leg trail")
    args = ap.parse_args()

    legs = load_legs(args.fixtures_dir, args.column, args.news)
    if not legs:
        print(f"no legs for column={args.column} news={args.news}", file=sys.stderr)
        return 1

    risk = args.risk_pct / 100.0
    concurrent = 0
    events = sorted([(l["entry"], 1) for l in legs] + [(l["exit"], -1) for l in legs])
    cur = 0
    for _, d in events:
        cur += d
        concurrent = max(concurrent, cur)

    print(f"column={args.column}  news={args.news}  legs={len(legs)}  "
          f"risk={args.risk_pct}%  max concurrent positions={concurrent}")
    if concurrent > 1 and not args.sequential:
        print("(sizing each entry off the balance at ITS entry time, so overlapping\n"
              " trades share a balance -- as they would live)")

    trail = simulate(legs, args.start, risk, args.sequential)
    mode = "sequential (optimistic)" if args.sequential else "overlap-aware"
    report(trail, args.start, risk, f"{args.column}  |  {mode}  |  {args.risk_pct}% per entry")
    bootstrap(legs, args.start, risk, args.bootstrap, args.seed)

    if args.verbose:
        print(f"\n  {'exit':<12} {'setup':<34} {'R':>7} {'risk$':>9} {'pnl$':>10} {'balance':>11}")
        for t in trail:
            print(f"  {str(t['exit'].date()):<12} {t['setup']:<34} {t['r']:>+7.2f} "
                  f"{t['risk_amount']:>9,.0f} {t['pnl']:>+10,.0f} {t['balance']:>11,.0f}")

    if args.csv:
        import csv
        with args.csv.open("w", newline="") as fh:
            w = csv.writer(fh)
            w.writerow(["exit", "entry", "setup", "r", "risk_amount", "pnl", "balance", "drawdown_pct"])
            for t in trail:
                w.writerow([t["exit"].isoformat(), t["entry"].isoformat(), t["setup"],
                            f"{t['r']:.4f}", f"{t['risk_amount']:.2f}", f"{t['pnl']:.2f}",
                            f"{t['balance']:.2f}", f"{t['drawdown_pct']:.2f}"])
        print(f"\nwrote {args.csv}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
