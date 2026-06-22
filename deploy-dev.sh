#!/usr/bin/env bash
# Deploy the DEV environment.
#
#   worker : trade-control-web-hook-dev       (branch: main)
#   CLIs   : trade-control-dev, tv-arm-dev, tv-news-dev, replay-candles-dev
#
# Every environment now carries a suffix. The old no-suffix worker
# `trade-control-web-hook` is deprecated (kept running only until last week's
# trades are journaled, then deleted) — this script targets the suffixed
# `-dev` worker.

set -euo pipefail

ENV_NAME="dev"
ENV_BRANCH="main"
ENV_WEBHOOK="https://trade-control-web-hook-dev.msherborne.workers.dev"
ENV_SUFFIX="dev"
# Pine study title tv-arm-dev arms against. Dev runs the newer Pine (v25,
# which sends `open` for M/W body-extreme logic). The chart study MUST be
# renamed to exactly this base title (the `(args)` suffix is ignored) or
# tv-arm-dev won't find it. See README "per-environment Pine versions".
ENV_PINE_NAME="Candle Signals v25"

source "$(dirname "$0")/deploy-lib.sh"
deploy_env "$ENV_NAME" "$ENV_BRANCH" "$ENV_WEBHOOK" "$ENV_SUFFIX" "$ENV_PINE_NAME"
