#!/usr/bin/env python3
"""Re-attach hand-written fixture `message` fields after a corpus regeneration.

A `message` is operator-authored prose (why this fixture exists, what it pins)
and is NOT reproducible from a re-run: `tv-arm --message` takes one string for
the whole matrix, so regenerating would flatten every cell of a setup to one
note, or blank it entirely. This restores them per CELL, by directory name.

New cells created by a new axis have no prior message. They inherit their
setup's message only when that setup had exactly ONE distinct message across
all its old cells — an unambiguous setup-level note. Where a setup carried
per-cell notes (different prose per column), a new cell is left blank rather
than guessing which note applies.

Usage: restore-messages.py <messages.json> <fixtures-dir> [--apply]
"""
import json, os, sys, glob
from collections import defaultdict

msgs = json.load(open(sys.argv[1]))
root = sys.argv[2]
apply_ = '--apply' in sys.argv

# setup -> the distinct messages its old cells carried
specs = sorted(os.path.basename(s)[:-len('.spec.json')]
               for s in glob.glob(f'{root}/*.spec.json'))
def setup_of(cell):
    m = [s for s in specs if cell.startswith(s + '-')]
    return max(m, key=len) if m else None

by_setup = defaultdict(set)
for cell, m in msgs.items():
    s = setup_of(cell)
    if s:
        by_setup[s].add(m)

exact = inherited = renamed = ambiguous = blank = 0
for f in sorted(glob.glob(f'{root}/*/meta.json')):
    cell = os.path.basename(os.path.dirname(f))
    want = msgs.get(cell)
    why = 'exact'
    if want is None:
        # A renamed cell: the axis appended a suffix, so the prose written for
        # the pre-axis cell still applies to the cell that kept its behaviour.
        # Try the name with each known axis suffix stripped, longest first, so
        # `-sl-fib-top-entry-stop` falls back to `-sl-fib-top` before the bare
        # name. Only `-entry-stop` and the unsuffixed default share behaviour
        # with the old cell; `-entry-market`/`-entry-limit` are genuinely new
        # cells and must NOT inherit a note describing a stop entry's fills.
        for suffix in ('-entry-stop',):
            if cell.endswith(suffix):
                want = msgs.get(cell[: -len(suffix)])
                if want is not None:
                    why = 'renamed'
                break
    if want is None:
        s = setup_of(cell)
        cands = by_setup.get(s, set())
        if len(cands) == 1:
            want = next(iter(cands)); why = 'inherited'
        elif len(cands) > 1:
            ambiguous += 1; continue
        else:
            blank += 1; continue
    d = json.load(open(f))
    if (d.get('message') or '') != want:
        d['message'] = want
        if apply_:
            json.dump(d, open(f, 'w'), indent=2)
            open(f, 'a').write('\n')
    if why == 'exact':
        exact += 1
    elif why == 'renamed':
        renamed += 1
    else:
        inherited += 1

print(f"{'APPLIED' if apply_ else 'DRY-RUN'}: exact={exact} inherited={inherited} "
      f"ambiguous-left-blank={ambiguous} no-prior-message={blank}")
