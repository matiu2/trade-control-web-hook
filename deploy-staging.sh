#!/usr/bin/env bash
# Deploy the STAGING environment (demo account).
#
#   worker : trade-control-web-hook-staging   (branch: staging)
#   CLIs   : trade-control-staging, tv-arm-staging, tv-news-staging
#
# Promotion: staging must run a full week unchanged + profitable before it
# is merged to prod. See DEPLOYED.md.

set -euo pipefail

ENV_NAME="staging"
ENV_BRANCH="staging"
ENV_WEBHOOK="https://trade-control-web-hook-staging.msherborne.workers.dev"
ENV_SUFFIX="staging"
# Pine study title tv-arm-staging arms against. Its chart study must be
# renamed to exactly this base title. Cut fresh from main on 2026-06-23
# (the prior staging week was unusable), so staging now tracks the current
# Pine v25 in lockstep with dev.
ENV_PINE_NAME="Candle Signals v25"

source "$(dirname "$0")/deploy-lib.sh"
deploy_env "$ENV_NAME" "$ENV_BRANCH" "$ENV_WEBHOOK" "$ENV_SUFFIX" "$ENV_PINE_NAME"
