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

source "$(dirname "$0")/deploy-lib.sh"
deploy_env "$ENV_NAME" "$ENV_BRANCH" "$ENV_WEBHOOK" "$ENV_SUFFIX"
