#!/usr/bin/env bash
# Deploy the STAGING environment (demo account).
#
#   worker : LOCAL native/Postgres worker on 127.0.0.1:8788  (branch: staging)
#   CLIs   : trade-control-staging, tv-arm-staging, tv-news-staging
#
# Staging now runs the LOCAL native/Postgres worker, NOT Cloudflare — Oracle
# Cloud compute (uk-london-1) is out of capacity, so this week's demo trading
# runs locally alongside dev (dev :8787 / staging :8788), each against its own
# Postgres database + dedicated role (staging → tc_staging/trade_control_staging).
# Like deploy-dev.sh, this does NOT `wrangler deploy`. It bakes each `-staging`
# CLI's default endpoint to the loopback worker, installs them, AND rolls the
# worker: rebuilds trade-control-worker, installs it to
# ~/.local/bin/trade-control-worker-staging, and restarts the systemd user
# service trade-control-worker-staging (see roll_native_worker in
# deploy-lib.sh). Secrets come from the service's EnvironmentFile
# (~/.config/trade-control/worker-secrets.env), not this script.
#
# ⚠️  Restarting rolls the LIVE demo worker — it briefly drops the process and
# reloads plan state from Postgres. Promotion gate: staging must run a full week
# unchanged + profitable before it is merged to prod, so don't redeploy staging
# casually mid-week. See DEPLOYED.md.

set -euo pipefail

ENV_NAME="staging"
ENV_BRANCH="staging"
# Local native/Postgres worker on loopback :8788 (dev is :8787). The suffixed
# `-staging` CLIs bake this as their default endpoint so no `--endpoint` needed.
ENV_WEBHOOK="http://127.0.0.1:8788"
ENV_SUFFIX="staging"

source "$(dirname "$0")/deploy-lib.sh"
# 5th arg "native" → skip wrangler deploy (local worker, CLIs only).
deploy_env "$ENV_NAME" "$ENV_BRANCH" "$ENV_WEBHOOK" "$ENV_SUFFIX" native
