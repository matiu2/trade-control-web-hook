#!/usr/bin/env python3
"""Paired comparison: skip-bcr with a MARKET main entry vs the corpus's STOP.

The corpus cell (`<setup>-skip-bcr-news-<x>`) and the new cell
(`<setup>-skip-bcr-market-news-<x>`) share a frozen spec, so the only
difference is the main entry's order type. Paired per setup; a setup missing
either side is reported, never silently dropped.

Usage:
    python3 scripts/compare-market-vs-stop-entry.py <market-fixtures-dir> [on|off]

The market-side cells are NOT in the committed corpus (a fifth column would
change every paired comparison in entry-rule-corpus-comparison.md). Recreate
them into a scratch dir with:

    while IFS=$'\t' read -r name inst _; do
      for mode in on off; do
        extra=""; [ "$mode" = off ] && extra="--skip-calendar-bars"
        tv-arm --spec-in "replay-fixtures/${name}.spec.json" \
          --skip-bcr --entry-market $extra \
          replay --save "${name}-skip-bcr-market-news-${mode}" --simulate true \
          --instrument "$inst" --fixtures-dir "$OUT"
      done
    done < jobs.txt   # jobs.txt: <setup-name>\t<instrument> per line

`--save-matrix` cannot be used here: it owns the entry-rule axis and rejects
`--skip-bcr`. Pass `--instrument` explicitly or replay silently pulls the
TradingView chart's symbol (see [[replay_candles_reads_chart_symbol]]).
"""
import json, os, sys, glob, statistics
from math import comb

CORPUS = 'replay-fixtures'
MKT = sys.argv[1] if len(sys.argv) > 1 else None
NEWS = sys.argv[2] if len(sys.argv) > 2 else 'on'

def outcome(p):
    try:
        o = json.load(open(p)).get('outcome') or {}
    except Exception:
        return None
    if o.get('net_r') is None:
        return None
    return {'r': float(o['net_r']), 'legs': len(o.get('legs') or []),
            'tp': o.get('tp_hits', 0), 'sl': o.get('sl_hits', 0)}

def entry_type(plan, qm=False):
    try:
        d = json.load(open(plan))
    except Exception:
        return None
    for r in d.get('rules', []):
        i = r.get('intent', {})
        if i.get('action') != 'enter':
            continue
        is_qm = str(i.get('id', '')).endswith('-qm')
        if is_qm == qm:
            return (i.get('entry') or {}).get('type')
    return None

rows, missing, badtype = [], [], []
for spec in sorted(glob.glob(f'{CORPUS}/*.spec.json')):
    name = os.path.basename(spec)[:-len('.spec.json')]
    stop_d = f'{CORPUS}/{name}-skip-bcr-news-{NEWS}'
    mkt_d = f'{MKT}/{name}-skip-bcr-market-news-{NEWS}'
    if not os.path.isdir(stop_d) or not os.path.isdir(mkt_d):
        if os.path.isdir(stop_d) or os.path.isdir(mkt_d):
            missing.append(name)
        continue
    s, m = outcome(stop_d + '/expected.json'), outcome(mkt_d + '/expected.json')
    if not s or not m:
        missing.append(name); continue
    ts, tm = entry_type(stop_d + '/plan.json'), entry_type(mkt_d + '/plan.json')
    if ts != 'stop' or tm != 'market':
        badtype.append((name, ts, tm)); continue
    rows.append((name, s, m))

print(f"paired setups (news={NEWS}) : {len(rows)}")
if missing:
    print(f"unpaired / unreadable      : {len(missing)}  {missing[:6]}")
if badtype:
    print(f"!! wrong entry types        : {len(badtype)}  {badtype[:4]}")
if not rows:
    sys.exit("nothing to compare")

d = [m['r'] - s['r'] for _, s, m in rows]
nz = [x for x in d if abs(x) > 1e-9]
better = sum(1 for x in nz if x > 0)
worse = sum(1 for x in nz if x < 0)
ts_, tm_ = sum(s['r'] for _, s, _ in rows), sum(m['r'] for _, _, m in rows)
print(f"\n{'':<28}{'STOP':>10}{'MARKET':>10}")
print(f"{'total R':<28}{ts_:>+10.2f}{tm_:>+10.2f}")
print(f"{'mean R':<28}{statistics.fmean([s['r'] for _,s,_ in rows]):>+10.2f}"
      f"{statistics.fmean([m['r'] for _,_,m in rows]):>+10.2f}")
print(f"{'median R':<28}{statistics.median([s['r'] for _,s,_ in rows]):>+10.2f}"
      f"{statistics.median([m['r'] for _,_,m in rows]):>+10.2f}")
print(f"{'filled legs':<28}{sum(s['legs'] for _,s,_ in rows):>10}"
      f"{sum(m['legs'] for _,_,m in rows):>10}")
print(f"{'TP / SL':<28}{str(sum(s['tp'] for _,s,_ in rows))+'/'+str(sum(s['sl'] for _,s,_ in rows)):>10}"
      f"{str(sum(m['tp'] for _,_,m in rows))+'/'+str(sum(m['sl'] for _,_,m in rows)):>10}")
print(f"\nsum delta (market - stop) : {sum(d):+.2f}")
print(f"setups differing          : {len(nz)} of {len(rows)}")
print(f"market better / worse     : {better} / {worse}")
if nz:
    print(f"median delta (differing)  : {statistics.median(nz):+.4f}")
    k, n = better, len(nz)
    p = sum(comb(n, i) for i in range(0, min(k, n - k) + 1)) * 2 / 2 ** n
    print(f"sign test p               : {min(p,1.0):.3f}")
    top = sorted(nz, key=abs, reverse=True)[:2]
    print(f"sum excl. top-2 outliers  : {sum(d) - sum(top):+.2f}")
print("\nlargest movers:")
for name, s, m in sorted(rows, key=lambda t: -abs(t[2]['r'] - t[1]['r']))[:12]:
    dd = m['r'] - s['r']
    if abs(dd) < 1e-9: break
    print(f"  {dd:+7.2f}  {name:<44} stop {s['r']:+6.2f} -> market {m['r']:+6.2f}")
