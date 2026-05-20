#!/usr/bin/env bash
# Refresh the worker's TN_SESSION_JSON secret from a locally-stored
# TradeNation account.
#
# Background: the worker can't run TradeNation's native login flow
# (the redirect chain that scrapes Set-Cookie). It uses a pre-built
# `Session` JSON stored as the `TN_SESSION_JSON` Cloudflare Worker
# secret. TradeNation sessions expire after a few hours of intermittent
# use, so this script does a fresh login + pushes the new session.
#
# Run it periodically from cron (every ~2 hours) and reactively
# whenever the worker logs `tradenation login failed`.
#
# Requirements:
#   - `tradenation` CLI installed and authenticated with the account
#   - `wrangler` CLI installed and logged in to the right Cloudflare
#     account (`wrangler login`, or CLOUDFLARE_API_TOKEN env var)
#   - The TN account name passed as TN_ACCOUNT_NAME, or override
#     by editing the default below.
#
# Usage:
#   refresh-tn-session.sh                      # uses TN_ACCOUNT_NAME below
#   TN_ACCOUNT_NAME="manual demo" refresh-tn-session.sh
#
# Cron suggestion (every 2 hours):
#   0 */2 * * * /home/matiu/projects/trading-libraries/trade-control-web-hook/scripts/refresh-tn-session.sh >> /tmp/tn-session-refresh.log 2>&1
#
# Exit codes:
#   0  session refreshed and pushed
#   1  tradenation export failed
#   2  wrangler push failed

set -euo pipefail

TN_ACCOUNT_NAME="${TN_ACCOUNT_NAME:-manual demo}"
WORKER_DIR="${WORKER_DIR:-$(cd "$(dirname "$0")/.." && pwd)}"

timestamp() { date -u +"%Y-%m-%dT%H:%M:%SZ"; }

echo "[$(timestamp)] refresh-tn-session: account='$TN_ACCOUNT_NAME' worker-dir='$WORKER_DIR'"

# Re-login and capture the session JSON. `session export` does a fresh
# login each call, so this is the same as rotating the secret manually.
if ! session_json="$(tradenation session export "$TN_ACCOUNT_NAME" 2>&1)"; then
    echo "[$(timestamp)] FAIL: tradenation session export: $session_json" >&2
    exit 1
fi

# Push to Cloudflare. `wrangler secret put` reads stdin.
if ! printf '%s' "$session_json" \
    | wrangler --cwd "$WORKER_DIR" secret put TN_SESSION_JSON 2>&1; then
    echo "[$(timestamp)] FAIL: wrangler secret put" >&2
    exit 2
fi

echo "[$(timestamp)] OK: TN_SESSION_JSON rotated"
