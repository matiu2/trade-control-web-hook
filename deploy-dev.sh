#!/usr/bin/env bash
# Deploy the DEV environment.
#
#   worker : trade-control-web-hook           (branch: main)
#   CLIs   : trade-control-dev, tv-arm-dev, tv-news-dev
#
# NOTE (promotion plan): next week `web-hook` becomes PROD and a fresh
# `web-hook-dev` worker is cut for dev. When that happens, change ENV_WEBHOOK
# below to the new dev URL — that's the only edit this script needs.

set -euo pipefail

ENV_NAME="dev"
ENV_BRANCH="main"
ENV_WEBHOOK="https://trade-control-web-hook.msherborne.workers.dev"
ENV_SUFFIX="dev"

source "$(dirname "$0")/deploy-lib.sh"
deploy_env "$ENV_NAME" "$ENV_BRANCH" "$ENV_WEBHOOK" "$ENV_SUFFIX"
