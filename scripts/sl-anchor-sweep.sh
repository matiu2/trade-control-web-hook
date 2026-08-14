#!/usr/bin/env bash
#
# Re-arm every frozen setup in the corpus across the three `--sl-anchor` values
# and replay each, so the stop-loss axis can be compared on setups already
# captured — without re-reading a single chart.
#
# WHY A SCRIPT AND NOT A RUST TOOL
# --------------------------------
# This drives the real `tv-arm` binary over `--spec-in`, which is the same path
# an operator's re-arm takes. A Rust re-implementation would be a second arm
# path that has to be kept in step with the first, and the moment it drifts the
# sweep is measuring the tool rather than the strategy.
#
# COVERAGE — READ THIS BEFORE TRUSTING THE OUTPUT
# -----------------------------------------------
# Only setups saved with `--spec-out` can be re-armed chartlessly. At the time
# of writing that is ~26 of ~206 fixture directories: the rest predate
# `--spec-out` and are NOT recoverable here — their geometry was never frozen.
# The script prints the covered count up front and again in the summary. It
# deliberately does not imply full-corpus coverage.
#
# To widen coverage, re-arm the missing setups from their charts once with
# `--spec-out`; from then on they join every future sweep.
#
# USAGE
#   scripts/sl-anchor-sweep.sh [--fixtures-dir DIR] [--out DIR] [--dry-run]
#
# Always passes `--fixtures-dir` through to the replay explicitly, because the
# default is easy to get wrong and a silently-wrong directory means a sweep that
# looks complete while writing nowhere useful.

set -euo pipefail

FIXTURES_DIR="${FIXTURES_DIR:-replay-fixtures}"
OUT_DIR=""
DRY_RUN=0
RESUME=0
TV_ARM="${TV_ARM:-./target/release/tv-arm}"

# One setup's SL cells: 2 structural anchors × 4 entry rules × news on/off.
# A setup with all of them present is complete and `--resume` skips it.
CELLS_PER_SETUP=16

while [[ $# -gt 0 ]]; do
  case "$1" in
    --fixtures-dir) FIXTURES_DIR="$2"; shift 2 ;;
    --out)          OUT_DIR="$2";      shift 2 ;;
    --dry-run)      DRY_RUN=1;         shift   ;;
    --resume)       RESUME=1;          shift   ;;
    -h|--help)      sed -n '2,30p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

OUT_DIR="${OUT_DIR:-$FIXTURES_DIR}"

# Used to restore tracked baseline cells the matrix rewrites (see the loop).
# Empty when the output isn't inside a git repo, which disables the restore —
# correct, since there is then nothing to restore from.
REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || true)"

if [[ ! -x "$TV_ARM" ]]; then
  echo "tv-arm binary not found at $TV_ARM" >&2
  echo "build it first:  cargo build --release -p tv-arm" >&2
  exit 4
fi

shopt -s nullglob
SPECS=("$FIXTURES_DIR"/*.spec.json)
shopt -u nullglob

if [[ ${#SPECS[@]} -eq 0 ]]; then
  echo "no *.spec.json under $FIXTURES_DIR — nothing to sweep" >&2
  echo "(only setups armed with --spec-out can be re-armed without a chart)" >&2
  exit 3
fi

TOTAL_DIRS=$(find "$FIXTURES_DIR" -mindepth 1 -maxdepth 1 -type d | wc -l | tr -d ' ')
echo "sl-anchor sweep"
echo "  specs found     : ${#SPECS[@]}"
echo "  fixture dirs    : ${TOTAL_DIRS}  (only the ${#SPECS[@]} with a spec can be re-armed)"
echo "  anchors         : signal, invalidation, fib-top"
echo "  cells per setup : 24  (4 entry rules × news on/off × 3 anchors)"
echo

ARMED=0
FAILED=0
SKIPPED=0
FAILED_NAMES=()

for spec in "${SPECS[@]}"; do
  name="$(basename "$spec" .spec.json)"

  # --resume: a setup whose 16 SL cells are all on disk is already done. The
  # sweep is long enough that losing it to a closed terminal is a real cost, and
  # re-arming a finished setup burns ~40s to reproduce identical output.
  if [[ $RESUME -eq 1 ]]; then
    have=$(find "$OUT_DIR" -mindepth 1 -maxdepth 1 -type d \
             \( -name "${name}-*-sl-invalidation" -o -name "${name}-*-sl-fib-top" \) \
           | wc -l | tr -d ' ')
    if [[ "$have" -ge $CELLS_PER_SETUP ]]; then
      SKIPPED=$((SKIPPED + 1))
      echo "· ${name}: ${have}/${CELLS_PER_SETUP} cells present — skipping (--resume)"
      continue
    fi
  fi
  # The instrument is frozen in the spec, but replay-candles must be told
  # explicitly — left implicit, the TV CHART's symbol silently wins.
  instrument="$(python3 -c "
import json,sys
d=json.load(open(sys.argv[1]))
sym=d.get('chart_symbol','')
print(sym.split(':')[-1] if sym else '')
" "$spec")"

  if [[ -z "$instrument" ]]; then
    echo "  ✗ ${name}: spec has no chart_symbol — skipping rather than guessing"
    FAILED=$((FAILED + 1)); FAILED_NAMES+=("$name"); continue
  fi

  echo "→ ${name}  (${instrument})"
  cmd=(
    "$TV_ARM"
    --spec-in "$spec"
    --save-matrix
    --sl-matrix
    replay
    --instrument "$instrument"
    --fixtures-dir "$OUT_DIR"
    --save "$name"
  )

  if [[ $DRY_RUN -eq 1 ]]; then
    printf '   would run:'; printf ' %q' "${cmd[@]}"; echo
    continue
  fi

  # ⚠ The 24-cell matrix rewrites the 8 BASELINE cells too (they're the
  # `signal` anchor), and a rewrite is destructive even when the verdict is
  # unchanged: `meta.json`'s `message` is a HAND-WRITTEN analysis note that the
  # replay cannot regenerate, so it comes back empty. Observed on the first full
  # sweep — 12 tracked cells rewritten, 6 hand-written notes lost, and every
  # `expected.json` byte-identical. Pure loss.
  #
  # Only the 16 new `-sl-*` cells are wanted here, so any tracked baseline cell
  # is restored from git afterwards. Untracked cells are left alone: git can't
  # restore them, and they carry no notes to lose.
  restore_tracked_baseline() {
    git -C "$REPO_ROOT" status --porcelain -- "$OUT_DIR" 2>/dev/null \
      | awk '$1=="M"{print $2}' \
      | grep -E "/${name}-[^/]*/(meta|plan|expected)\.json$" \
      | grep -vE '\-sl\-(invalidation|fib-top)/' \
      | while read -r tracked; do
          git -C "$REPO_ROOT" checkout -- "$tracked" 2>/dev/null || true
        done
  }

  # A setup that fails to arm is recorded and the sweep continues — the same
  # discipline the matrix itself uses. One bad spec must not cost the other 25.
  if "${cmd[@]}"; then
    ARMED=$((ARMED + 1))
  else
    echo "  ✗ ${name}: arm/replay returned non-zero"
    FAILED=$((FAILED + 1)); FAILED_NAMES+=("$name")
  fi
  # Runs on success AND failure: a partial run can still have rewritten a
  # baseline cell before it died.
  restore_tracked_baseline
done

echo
echo "sl-anchor sweep: ${ARMED}/${#SPECS[@]} setup(s) swept"
if [[ $SKIPPED -gt 0 ]]; then
  # Named explicitly: a resumed run that reported only "swept" would look like a
  # partial sweep, and one that silently counted skips as swept would look like
  # a full one. Neither is true.
  echo "  skipped (already complete): ${SKIPPED}"
  echo "  covered = swept + skipped  = $((ARMED + SKIPPED))/${#SPECS[@]}"
fi
if [[ $FAILED -gt 0 ]]; then
  echo "  failed: ${FAILED_NAMES[*]}"
fi
echo "  NOTE: ${#SPECS[@]} of ${TOTAL_DIRS} fixture dirs have a frozen spec;"
echo "        the rest predate --spec-out and cannot be re-armed without a chart."

# Non-zero when anything failed: a partial sweep must not read as a complete one
# to a driver that only checks the exit code.
[[ $FAILED -eq 0 ]]
