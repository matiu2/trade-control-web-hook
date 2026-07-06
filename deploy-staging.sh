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
# Like deploy-dev.sh, this does NOT `wrangler deploy`; it only bakes each
# `-staging` CLI's default endpoint to the loopback worker and installs them.
# The worker process itself is long-running, managed outside this script:
#
#   SIGNING_KEY="$(tr -d '[:space:]' < ~/.config/trade-control/key.hex)" \
#   ADMIN_KEY="$(tr -d '[:space:]' < ~/.config/trade-control/admin-key.hex)" \
#     ./target/release/trade-control-worker ~/.config/trade-control/staging-worker.toml
#
# Promotion: staging must run a full week unchanged + profitable before it
# is merged to prod. See DEPLOYED.md.

set -euo pipefail

ENV_NAME="staging"
ENV_BRANCH="staging"
# Local native/Postgres worker on loopback :8788 (dev is :8787). The suffixed
# `-staging` CLIs bake this as their default endpoint so no `--endpoint` needed.
ENV_WEBHOOK="http://127.0.0.1:8788"
ENV_SUFFIX="staging"
# Pine study title tv-arm-staging arms against. Its chart study must be
# renamed to exactly this base title. Cut fresh from main on 2026-06-23
# (the prior staging week was unusable), so staging now tracks the current
# Pine v25 in lockstep with dev.
ENV_PINE_NAME="Candle Signals v25"

source "$(dirname "$0")/deploy-lib.sh"
# 6th arg "native" → skip wrangler deploy (local worker, CLIs only).
deploy_env "$ENV_NAME" "$ENV_BRANCH" "$ENV_WEBHOOK" "$ENV_SUFFIX" "$ENV_PINE_NAME" native
