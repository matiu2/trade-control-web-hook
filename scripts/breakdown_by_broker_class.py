#!/usr/bin/env python3
"""Break the corpus down by BROKER and by ASSET CLASS for one entry rule.

Answers "where is the profit actually coming from" — the operator trades
TradeNation FX in practice, so the question is whether the corpus result
survives that restriction.

Asset class comes from **`instrument-lookup`**, the canonical catalog, never
from pattern-matching the symbol. The corpus records instruments in whichever
broker spelling was armed (`EUR/CAD` on TradeNation, `EUR_CAD` on OANDA), and
`resolve` collapses both onto one canonical id + class. A symbol the catalog
does not know is reported as UNRESOLVED and excluded from the class table
rather than being guessed into a bucket.

⚠️ Broker here is the **candle source the fixture was armed against**
(`meta.json` `source` / `arm.candle_source`), not necessarily where the trade
was placed live. Comparing brokers therefore compares *price feeds and their
spreads*, which is the honest reading of what the corpus holds.

Setups are collapsed before aggregation: a setup can contribute several legs
(multi-shot), and R is summed per setup so one busy setup doesn't outvote the
rest of its bucket.
"""

from __future__ import annotations

import argparse
import json
import statistics
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

COLUMNS = ["normal", "skip-bcr", "strategy-v2", "strategy-v2-qm-market"]

# Catalog classes grouped the way the operator thinks about them. `gold` is
# kept separate from `commodity` by the catalog; both are commodities to a
# trader, so they are grouped but the split is still printed.
GROUP = {
    "forex": "Forex",
    "index": "Indices",
    "commodity": "Commodities",
    "gold": "Commodities",
    "crypto": "Crypto",
    "bond": "Bonds",
    "stock": "Stock CFDs",
}

_cache: dict[str, tuple[str, str] | None] = {}


def resolve(symbol: str):
    """(canonical_id, class) from instrument-lookup, or None if unknown.

    A non-zero exit is load-bearing information ("not in the catalog"), so it
    is surfaced as UNRESOLVED rather than swallowed into a default bucket.
    """
    if symbol in _cache:
        return _cache[symbol]
    try:
        out = subprocess.run(
            ["instrument-lookup", "resolve", symbol, "--json"],
            capture_output=True, text=True, timeout=15,
        )
        if out.returncode != 0:
            _cache[symbol] = None
        else:
            d = json.loads(out.stdout)
            _cache[symbol] = (d["id"], d.get("class", "unknown"))
    except (OSError, json.JSONDecodeError, KeyError, subprocess.TimeoutExpired):
        _cache[symbol] = None
    return _cache[symbol]


def load(root: Path, column: str, news: str):
    """One row per setup: broker, instrument, class, summed R, leg count."""
    rows, unresolved = [], set()
    tail = f"-{column}-news-{news}"
    for d in sorted(root.glob("*/")):
        if not d.name.endswith(tail) or "-sl-" in d.name:
            continue
        try:
            meta = json.loads((d / "meta.json").read_text())
            exp = json.loads((d / "expected.json").read_text())
        except (OSError, json.JSONDecodeError):
            continue
        outcome = exp.get("outcome") or {}
        if outcome.get("net_r") is None:
            continue
        symbol = meta.get("instrument", "")
        got = resolve(symbol)
        if got is None:
            unresolved.add(symbol)
            cid, cls = symbol, None
        else:
            cid, cls = got
        rows.append({
            "setup": d.name[: -len(tail)],
            "broker": (meta.get("source") or "?").lower(),
            "symbol": symbol,
            "id": cid,
            "cls": cls,
            "group": GROUP.get(cls, "Other") if cls else None,
            "r": float(outcome["net_r"]),
            "legs": len(outcome.get("legs") or []),
        })
    return rows, unresolved


def table(rows, key, title, note=None):
    buckets = defaultdict(list)
    for r in rows:
        if r[key] is not None:
            buckets[r[key]].append(r)
    if not buckets:
        print(f"\n{title}: nothing to show")
        return
    print(f"\n{title}")
    if note:
        print(f"  {note}")
    print(f"  {'bucket':<16} {'setups':>7} {'traded':>7} {'totalR':>9} {'R/setup':>9} {'R/trade':>9} {'win%':>6}")
    print("  " + "-" * 68)
    for name in sorted(buckets, key=lambda b: -sum(x["r"] for x in buckets[b])):
        g = buckets[name]
        traded = [x for x in g if x["legs"] > 0]
        tot = sum(x["r"] for x in g)
        wins = sum(1 for x in traded if x["r"] > 0)
        rpt = tot / len(traded) if traded else 0.0
        wr = wins / len(traded) * 100 if traded else 0.0
        print(f"  {name:<16} {len(g):>7} {len(traded):>7} {tot:>+9.2f} "
              f"{tot/len(g):>+9.2f} {rpt:>+9.2f} {wr:>5.0f}%")


def main():
    root = Path(__file__).resolve().parent.parent / "replay-fixtures"
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--fixtures-dir", type=Path, default=root)
    ap.add_argument("--column", default="skip-bcr", choices=COLUMNS)
    ap.add_argument("--news", default="on", choices=["on", "off"])
    ap.add_argument("--broker", help="restrict to one broker (e.g. tradenation)")
    ap.add_argument("-v", "--verbose", action="store_true", help="per-setup rows")
    args = ap.parse_args()

    rows, unresolved = load(args.fixtures_dir, args.column, args.news)
    if not rows:
        print("no rows", file=sys.stderr)
        return 1
    if args.broker:
        rows = [r for r in rows if r["broker"] == args.broker]

    tot = sum(r["r"] for r in rows)
    traded = [r for r in rows if r["legs"] > 0]
    print(f"column={args.column}  news={args.news}"
          + (f"  broker={args.broker}" if args.broker else ""))
    print(f"setups={len(rows)}  traded={len(traded)}  totalR={tot:+.2f}")
    if unresolved:
        print(f"\n⚠️  UNRESOLVED by instrument-lookup (excluded from the class table): "
              f"{sorted(unresolved)}")

    table(rows, "broker", "BY BROKER (the candle source the fixture was armed against)")
    table(rows, "group", "BY ASSET CLASS")
    table(rows, "cls", "BY CATALOG CLASS (raw, gold split out from commodity)")

    # The operator's actual practice: TradeNation forex only.
    tn_fx = [r for r in rows if r["broker"] == "tradenation" and r["cls"] == "forex"]
    if tn_fx:
        t = sum(r["r"] for r in tn_fx)
        td = [r for r in tn_fx if r["legs"] > 0]
        print(f"\nTRADENATION FOREX ONLY — what is actually being traded now")
        print(f"  setups {len(tn_fx)}   traded {len(td)}   totalR {t:+.2f}   "
              f"R/setup {t/len(tn_fx):+.2f}   R/trade {t/len(td) if td else 0:+.2f}")
        rest = [r for r in rows if not (r["broker"] == "tradenation" and r["cls"] == "forex")]
        if rest:
            tr = sum(r["r"] for r in rest)
            print(f"  everything else: setups {len(rest)}  totalR {tr:+.2f}  "
                  f"R/setup {tr/len(rest):+.2f}")

    if args.verbose:
        print(f"\n  {'R':>7} {'legs':>5}  {'broker':<12} {'class':<11} {'id':<12} setup")
        for r in sorted(rows, key=lambda x: -x["r"]):
            print(f"  {r['r']:>+7.2f} {r['legs']:>5}  {r['broker']:<12} "
                  f"{str(r['cls']):<11} {r['id']:<12} {r['setup']}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
