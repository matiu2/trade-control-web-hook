#!/usr/bin/env bash
# Record one deploy as a page in the bug journal.
#
# Called at the end of `deploy_env` (and on failure, via the caller's trap) so
# every roll of an environment leaves a dated, immutable record of WHICH BUILD
# of every component was running from that moment until the next deploy.
#
# ## Why this exists
#
# The operator's words: "Currently I'm finding bugs, but not sure if they have
# been fixed already." A bug report says what went wrong; it cannot say which
# build it went wrong ON unless something wrote that down at the time. Trades
# outlive deploys — a plan armed on Monday can still be resting on Thursday,
# across two worker rolls — so "the version running now" is not the version
# that armed it, and by the time a bug surfaces the binaries have moved on.
#
# The components already self-report (`tv-arm-staging --version` ->
# `tv-arm v148-3-g71b70269`). Nothing was capturing that. This does.
#
# ## What it deliberately does NOT do
#
# It does not BUMP any version. A deploy records what `git describe` already
# says; it never creates a tag. Auto-tagging on deploy would make `vNN` mean
# "a time the script ran" instead of "a release someone decided to make", and
# the CHANGELOG convention (Why / What changed / Breaking / …) depends on that
# distinction.
#
# Usage: record-deploy.sh <env-name> <suffix> <status>
#   env-name : staging | dev
#   suffix   : the CLI suffix (staging, dev)
#   status   : ok | failed
#
# Never fails the deploy. A recorder that can abort a deploy is worse than no
# recorder — every failure path here degrades to a note in the page.

set -uo pipefail   # NOT -e: see above. A missing tool must not kill the deploy.

ENV_NAME="${1:?usage: record-deploy.sh <env-name> <suffix> <status>}"
SUFFIX="${2:?}"
STATUS="${3:-ok}"

JOURNAL="${DEPLOY_JOURNAL:-$HOME/projects/the-trading-academy/books/bugs-journal}"
PAGES="$JOURNAL/src/deploys"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LOCAL_CHART="${LOCAL_CHART_REPO:-$HOME/projects/trading-libraries/local-chart}"

# Brisbane (UTC+10, no DST) — the house timezone for every journal timestamp.
STAMP="$(TZ=Australia/Brisbane date '+%Y-%m-%d-%H%M')"
HUMAN="$(TZ=Australia/Brisbane date '+%Y-%m-%d %H:%M %Z')"
PAGE="$PAGES/$STAMP-$ENV_NAME.md"

mkdir -p "$PAGES" || { echo "record-deploy: cannot create $PAGES — skipping" >&2; exit 0; }

# `git describe` for a repo, with the dirty flag. Empty string if not a repo.
describe() {
  git -C "$1" describe --tags --dirty --always 2>/dev/null
}

branch_of() {
  git -C "$1" rev-parse --abbrev-ref HEAD 2>/dev/null
}

# A binary's own `--version`, or a marker saying why not.
version_of() {
  local bin="$1"
  if ! command -v "$bin" >/dev/null 2>&1; then
    echo "NOT INSTALLED"
    return
  fi
  "$bin" --version 2>/dev/null | head -1 || echo "no --version"
}

{
  echo "# Deploy — $ENV_NAME — $HUMAN"
  echo
  if [[ "$STATUS" != "ok" ]]; then
    echo "> ## ⚠️  THIS DEPLOY **FAILED**"
    echo ">"
    echo "> The run did not complete. Some components may have been installed and"
    echo "> others not, so the versions below are what was on disk when it stopped —"
    echo "> NOT a coherent set. Treat a trade armed in this window with suspicion,"
    echo "> and check the next successful deploy page for the state it was repaired to."
    echo
  fi

  echo '## Component versions'
  echo
  echo 'What each binary reports for itself. These are the builds that were live'
  echo 'from this moment until the next deploy page.'
  echo
  echo '| component | version |'
  echo '|---|---|'
  for b in tv-arm trade-control replay-candles journal tv-news; do
    printf '| `%s-%s` | `%s` |\n' "$b" "$SUFFIX" "$(version_of "$b-$SUFFIX")"
  done
  # The worker has no `--version` (it takes a positional config path, so the
  # flag is parsed as a filename and errors). Until it grows one, identify the
  # installed binary by mtime + size — weaker than a git describe, but it does
  # distinguish one build from another, which is the question being asked.
  worker_bin="$HOME/.local/bin/trade-control-worker-$SUFFIX"
  if [[ -x "$worker_bin" ]]; then
    printf '| `trade-control-worker-%s` | built %s, %s bytes _(no `--version` yet)_ |\n' \
      "$SUFFIX" \
      "$(TZ=Australia/Brisbane date -r "$worker_bin" '+%Y-%m-%d %H:%M' 2>/dev/null || echo '?')" \
      "$(stat -c%s "$worker_bin" 2>/dev/null || echo '?')"
  else
    printf '| `trade-control-worker-%s` | NOT INSTALLED |\n' "$SUFFIX"
  fi
  printf '| `local-chart` | `%s` |\n' \
    "$("$LOCAL_CHART/target/release/local-chart" --version 2>/dev/null | head -1 || echo 'NOT BUILT')"
  echo

  echo '## Source'
  echo
  echo '| repo | branch | describe |'
  echo '|---|---|---|'
  printf '| `trade-control-web-hook` | `%s` | `%s` |\n' "$(branch_of "$REPO_ROOT")" "$(describe "$REPO_ROOT")"
  printf '| `local-chart` | `%s` | `%s` |\n' "$(branch_of "$LOCAL_CHART")" "$(describe "$LOCAL_CHART")"
  echo
  echo 'A `-dirty` suffix means the working tree had uncommitted changes at build'
  echo 'time — the commit named does NOT fully describe what was deployed.'
  echo

  echo '## Changes since the previous deploy'
  echo
  prev="$(ls -1 "$PAGES"/*-"$ENV_NAME".md 2>/dev/null | grep -v "$(basename "$PAGE")" | tail -1)"
  prev_sha="$(grep -oP '^<!-- tcwh-sha: \K[0-9a-f]+' "$prev" 2>/dev/null | head -1)"
  if [[ -n "$prev_sha" ]]; then
    echo "Commits on \`trade-control-web-hook\` since the last **$ENV_NAME** deploy"
    echo "([\`${prev_sha:0:9}\`](#), $(basename "$prev" .md)):"
    echo
    delta="$(git -C "$REPO_ROOT" log --oneline "$prev_sha..HEAD" 2>/dev/null | head -50)"
    if [[ -n "$delta" ]]; then
      echo '```'
      echo "$delta"
      echo '```'
    else
      echo '_No commits since that deploy — this is a redeploy of the same source._'
    fi
  else
    echo "_No previous **$ENV_NAME** deploy page found — this is the baseline._"
  fi
  echo

  echo '## Plans in flight at deploy time'
  echo
  echo 'A snapshot from `journal-'"$SUFFIX"' --dump`, taken BEFORE the restart. Any'
  echo 'plan listed here crossed the version boundary: it was armed by the previous'
  echo 'deploy and continued under this one, so a bug affecting it may belong to'
  echo 'either.'
  echo
  echo '```'
  timeout 90 "journal-$SUFFIX" --dump 2>&1 | head -60 || echo '(journal dump unavailable)'
  echo '```'
  echo

  echo '<!-- Machine-readable anchors for the next run. Do not edit. -->'
  echo "<!-- tcwh-sha: $(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null) -->"
  echo "<!-- status: $STATUS -->"
} > "$PAGE" 2>/dev/null

echo "==> [$ENV_NAME] deploy recorded: $PAGE"

# Rebuild the SUMMARY so the page is reachable in the rendered book. Sorted
# newest-first: the most recent deploy is the one you reach for when a bug
# lands, so it should not be at the bottom of a growing list.
SUMMARY="$JOURNAL/src/SUMMARY.md"
if [[ -f "$SUMMARY" ]]; then
  python3 - "$SUMMARY" "$PAGES" <<'PY' 2>/dev/null || echo "record-deploy: SUMMARY not updated" >&2
import os, re, sys
summary, pages = sys.argv[1], sys.argv[2]
rows = []
for f in sorted(os.listdir(pages), reverse=True):
    if not f.endswith('.md'):
        continue
    title = f[:-3]
    first = ''
    try:
        with open(os.path.join(pages, f), encoding='utf-8') as fh:
            for line in fh:
                if line.startswith('# '):
                    first = line[2:].strip()
                    break
    except OSError:
        pass
    failed = ''
    try:
        with open(os.path.join(pages, f), encoding='utf-8') as fh:
            if 'status: failed' in fh.read():
                failed = ' ⚠️ FAILED'
    except OSError:
        pass
    rows.append(f'  - [{first or title}{failed}](./deploys/{f})')

block = '## Deploys\n\nOne page per deploy: which build of every component was live, and which\nplans crossed the boundary. Newest first.\n\n' + '\n'.join(rows) + '\n'

text = open(summary, encoding='utf-8').read()
if '## Deploys' in text:
    text = re.sub(r'## Deploys\n.*?(?=\n## |\Z)', block, text, flags=re.S)
else:
    text = text.rstrip() + '\n\n' + block
open(summary, 'w', encoding='utf-8').write(text)
PY
fi

exit 0
