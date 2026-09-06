#!/usr/bin/env bash
# Find prep pairs poisoned by BUG C (identical `set_at`), and say whether any
# belongs to a LIVE plan.
#
# BUG C: preps used to be stamped with wall-clock `now`, so a cron tick that
# caught up over several bars stamped every prep it wrote with ONE instant. The
# entry gate requires STRICTLY increasing prep times, so identical stamps read
# as out-of-order and the entry is rejected `prep-order-violated` on correct
# geometry. Fixed by stamping `set_at` from the bar (`verified.shell.time`).
#
# The code fix stops NEW poisoning; it does not clean up rows already written.
# Preps carry ~100-year TTLs, so poisoned pairs survive indefinitely. A poisoned
# pair on a live plan keeps rejecting that plan's entries even after deploy,
# until those two rows are cleared.
#
# Run this BEFORE deploying the fix. Exit 0 = nothing live is affected.
#         Exit 3 = a LIVE plan is affected; clear those preps (see below).
set -euo pipefail

DB="${1:-trade_control_staging}"
PSQL=(psql -U tc_staging -h 127.0.0.1 -d "$DB" -tAc)

PAIRS_SQL="
SELECT p1.account, p1.instrument, p1.set_at, p1.setter_id, p2.setter_id
FROM prep p1 JOIN prep p2
  ON p1.instrument = p2.instrument
 AND COALESCE(p1.account,'') = COALESCE(p2.account,'')
 AND p1.step < p2.step
 AND p1.set_at = p2.set_at
ORDER BY p1.set_at DESC;"

LIVE_SQL="
SELECT DISTINCT ps.trade_id
FROM plan_state ps
JOIN prep p1 ON p1.setter_id LIKE ps.trade_id || '%'
JOIN prep p2
  ON p2.instrument = p1.instrument
 AND COALESCE(p2.account,'') = COALESCE(p1.account,'')
 AND p1.step < p2.step
 AND p1.set_at = p2.set_at;"

echo "== Poisoned prep pairs (identical set_at) in ${DB} =="
pairs="$("${PSQL[@]}" "$PAIRS_SQL")"
if [ -z "$pairs" ]; then
  echo "  none"
else
  echo "$pairs" | sed 's/^/  /'
fi

echo
echo "== Of those, plans that are still LIVE =="
live="$("${PSQL[@]}" "$LIVE_SQL")"
if [ -z "$live" ]; then
  echo "  none — no cleanup needed, deploy freely"
  exit 0
fi

echo "$live" | sed 's/^/  /'
cat <<'MSG'

⚠️  These plans carry poisoned preps and will KEEP rejecting entries after the
    fix deploys, because the bad rows are already written. Clear each plan's
    two prep rows so they are re-stamped from the bar on the next fire:

      DELETE FROM prep WHERE setter_id LIKE '<trade_id>%';

    Do this only for the plans listed above.
MSG
exit 3
