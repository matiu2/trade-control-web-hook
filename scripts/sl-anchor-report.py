#!/usr/bin/env python3
"""Summarise the `--sl-anchor` sweep: net R per stop-loss anchor.

Reads the fixture corpus written by `scripts/sl-anchor-sweep.sh` and groups the
`expected.json` verdicts by (entry rule, news on/off, SL anchor), so the three
anchors can be compared on setups that are otherwise identical.

WHAT IT WILL AND WON'T TELL YOU
-------------------------------
It compares only the setups that have all three anchors, so a setup whose
structural cells failed to arm can't quietly drag one column down. The covered
count is printed; the corpus is ~26 frozen specs, not the full ~206 fixture
dirs, so treat this as a directional read on a small sample rather than a
verdict. A single setup can dominate a mean at this n — the per-setup table is
there so an outlier is visible rather than averaged away.
"""

import json
import os
import re
import sys
from collections import defaultdict

FIXTURES = sys.argv[1] if len(sys.argv) > 1 else "replay-fixtures"

# Cell directory names look like:
#   <setup>-<entry-rule>-news-<on|off>[-sl-<anchor>]
# The SL suffix is absent for the default `signal` anchor, which is exactly why
# the existing corpus keeps its original names.
CELL = re.compile(
    r"^(?P<setup>.+?)-(?P<rule>normal|skip-bcr|strategy-v2-qm-market|strategy-v2)"
    r"-news-(?P<news>on|off)(?:-sl-(?P<anchor>invalidation|fib-top))?$"
)


def verdict(cell_dir):
    """`outcome` for one cell, or None when the fixture has no verdict.

    Read from `expected.json`'s `outcome` block, which carries `net_r` plus the
    exit-reason breakdown (`sl_hits`, `tp_hits`, `reversal_closes`, ...). The
    breakdown matters more than net R for the stop-loss question: "did the tight
    stop get clipped" is `sl_hits`, and a net-R difference that isn't visible
    there came from somewhere other than the stop.
    """
    path = os.path.join(cell_dir, "expected.json")
    if not os.path.isfile(path):
        return None
    try:
        with open(path) as fh:
            data = json.load(fh)
    except (OSError, json.JSONDecodeError):
        return None
    out = data.get("outcome")
    if not isinstance(out, dict) or not isinstance(out.get("net_r"), (int, float)):
        return None
    return out


def news_axis_table(cells):
    """What the news standoff actually cost or earned, per cell.

    Only meaningful for fixtures written after 2026-08-15. Before that the
    `news-off` cells armed the news rules anyway (the flag was consumed upstream
    of the shared `SetupInputs`), so every pair matched by construction and this
    table would read as a confident "news never matters".

    Expect most pairs to still match even now: a pause blocks *new entries* and
    pulls *resting orders*, but does not close a filled position — so a window
    that opens while a position is already filled changes the fire log and not
    the R. The rows that DO differ are the whole point; they are the only direct
    evidence the corpus carries about whether standing aside for news pays.
    """
    diffs = []
    same = 0
    for (rule, news, anchor), per_setup in cells.items():
        if news != "on":
            continue
        off = cells.get((rule, "off", anchor), {})
        for setup, v in per_setup.items():
            if setup not in off:
                continue
            d = v["net_r"] - off[setup]["net_r"]
            if abs(d) < 1e-9:
                same += 1
            else:
                diffs.append((d, setup, rule, anchor, v["net_r"], off[setup]["net_r"]))

    print()
    print("news axis — where standing aside for news changed the result")
    print("-" * 72)
    if not diffs and not same:
        print("  no comparable news-on/news-off pairs found")
        return
    if not diffs:
        print(f"  {same} pair(s) compared, none differ.")
        print("  If this reads 0-of-everything on a FRESH sweep, suspect the axis")
        print("  again (see save_matrix's module doc) rather than concluding news")
        print("  is irrelevant — that is exactly how the dead axis presented.")
        return

    diffs.sort(reverse=True)
    print(f"  {len(diffs)} of {len(diffs) + same} pair(s) differ")
    print(f"  {'setup':30}{'rule':24}{'anchor':14}{'on':>8}{'off':>8}{'Δ':>8}")
    for d, setup, rule, anchor, on, off_r in diffs:
        print(f"  {setup[:30]:30}{rule:24}{anchor:14}{on:+8.2f}{off_r:+8.2f}{d:+8.2f}")
    net = sum(d for d, *_ in diffs)
    verb = "EARNED" if net > 0 else "COST"
    print(f"\n  net effect of the news standoff: {net:+.2f}R  ({verb} across differing cells)")


def main():
    # (rule, news, anchor) -> {setup: net_r}
    cells = defaultdict(dict)
    for name in sorted(os.listdir(FIXTURES)):
        full = os.path.join(FIXTURES, name)
        if not os.path.isdir(full):
            continue
        m = CELL.match(name)
        if not m:
            continue
        v = verdict(full)
        if v is None:
            continue
        anchor = m["anchor"] or "signal"
        cells[(m["rule"], m["news"], anchor)][m["setup"]] = v

    if not cells:
        print(f"no matching fixture cells under {FIXTURES}", file=sys.stderr)
        return 3

    anchors = ["signal", "invalidation", "fib-top"]
    print(f"{'entry rule':24} {'news':5} {'n':>3}  " + "".join(f"{a:>14}" for a in anchors))
    print("-" * 72)

    for rule in ["normal", "skip-bcr", "strategy-v2", "strategy-v2-qm-market"]:
        for news in ["on", "off"]:
            per = {a: cells.get((rule, news, a), {}) for a in anchors}
            # Only setups present under ALL three anchors — otherwise a column
            # with fewer setups looks better or worse purely by which it has.
            common = set.intersection(*(set(p) for p in per.values())) if all(per.values()) else set()
            if not common:
                continue
            row = f"{rule:24} {news:5} {len(common):3}  "
            for a in anchors:
                row += f"{sum(per[a][s]['net_r'] for s in common):+14.2f}"
            print(row)

    # Totals, NEWS-ON ONLY. Summing both sides of the news axis would count every
    # setup twice — and while the axis was dead (before 2026-08-15 every
    # `news-off` cell armed the news rules anyway) the two halves were identical,
    # so the doubled total was exactly 2× the real number and looked plausible.
    # news-on is the live-worker configuration, so it is the honest single view.
    print("-" * 72)
    totals = {a: 0.0 for a in anchors}
    setups = set()
    groups = {(rule, news) for (rule, news, _) in cells if news == "on"}
    for rule, news in groups:
        per = {a: cells.get((rule, news, a), {}) for a in anchors}
        if not all(per.values()):
            continue
        common = set.intersection(*(set(p) for p in per.values()))
        for a in anchors:
            totals[a] += sum(per[a][s]["net_r"] for s in common)
        setups |= common
    print(f"{'TOTAL (news-ON only)':24} {'':5} {len(setups):3}  "
          + "".join(f"{totals[a]:+14.2f}" for a in anchors))
    print(f"  ({len(setups)} distinct setup(s) × up to 4 entry rules; news-off excluded")
    print("   so setups aren't counted twice — see the news-axis table below.)")

    news_axis_table(cells)

    # Why the totals differ. A stop-loss change should show up as stop-outs
    # traded for something else; a net-R gap with identical `sl_hits` came from
    # somewhere other than the stop and is worth a second look.
    print()
    print(f"{'exit reason (news-ON)':24} {'':5} {'':3}  " + "".join(f"{a:>14}" for a in anchors))
    print("-" * 72)
    for label, key in [
        ("stopped out", "sl_hits"),
        ("take profit", "tp_hits"),
        ("reversal close", "reversal_closes"),
        ("expiry close", "expiry_closes"),
        ("invalidation close", "invalidation_closes"),
        ("open at end", "open_at_end"),
    ]:
        counts = {a: 0 for a in anchors}
        for rule, news in groups:
            per = {a: cells.get((rule, news, a), {}) for a in anchors}
            if not all(per.values()):
                continue
            common = set.intersection(*(set(p) for p in per.values()))
            for a in anchors:
                counts[a] += sum(int(per[a][s].get(key, 0)) for s in common)
        print(f"{label:24} {'':5} {'':3}  " + "".join(f"{counts[a]:14d}" for a in anchors))

    print()
    print("Common setups only: a setup missing any anchor is excluded from that row,")
    print("so the columns are always compared on identical setups.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
