#!/usr/bin/env bash
# Deploy the DEV environment.
#
#   worker : LOCAL native/Postgres worker on 127.0.0.1:8787  (branch: main)
#   CLIs   : trade-control-dev, tv-arm-dev, tv-news-dev, replay-candles-dev
#
# Dev runs the local native/Postgres worker, NOT Cloudflare. (Cloudflare is
# fully retired — staging is native/Postgres too now.) So this script does NOT
# `wrangler deploy` — it only bakes each `-dev` CLI's default endpoint to the
# loopback worker and installs them.
# The local worker itself is a long-running process managed outside this script:
#
#   SIGNING_KEY="$(tr -d '[:space:]' < ~/.config/trade-control/key.hex)" \
#   ADMIN_KEY="$(tr -d '[:space:]' < ~/.config/trade-control/admin-key.hex)" \
#     ./target/release/trade-control-worker <config.toml>

set -euo pipefail

ENV_NAME="dev"
ENV_BRANCH="main"
# Dev is the LOCAL native/Postgres worker (127.0.0.1:8787), not Cloudflare
# (which is fully retired — staging is local too). The suffixed `-dev` CLIs
# bake this as their default endpoint so no `--endpoint` flag is needed.
ENV_WEBHOOK="http://127.0.0.1:8787"
ENV_SUFFIX="dev"
# Pine study title tv-arm-dev arms against. Dev runs the newer Pine (v25,
# which sends `open` for M/W body-extreme logic). The chart study MUST be
# renamed to exactly this base title (the `(args)` suffix is ignored) or
# tv-arm-dev won't find it. See README "per-environment Pine versions".
ENV_PINE_NAME="Candle Signals v25"

source "$(dirname "$0")/deploy-lib.sh"
# 6th arg "native" → skip wrangler deploy (local worker, CLIs only).
deploy_env "$ENV_NAME" "$ENV_BRANCH" "$ENV_WEBHOOK" "$ENV_SUFFIX" "$ENV_PINE_NAME" native
