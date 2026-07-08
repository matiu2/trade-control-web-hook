#!/usr/bin/env bash
# Deploy the DEV environment.
#
#   worker : LOCAL native/Postgres worker on 127.0.0.1:8787  (branch: main)
#   CLIs   : trade-control-dev, tv-arm-dev, tv-news-dev, replay-candles-dev
#
# Dev runs the local native/Postgres worker, NOT Cloudflare. (Cloudflare is
# fully retired — staging is native/Postgres too now.) So this script does NOT
# `wrangler deploy`. It bakes each `-dev` CLI's default endpoint to the loopback
# worker, installs them, AND rolls the worker itself: rebuilds
# trade-control-worker, installs it to ~/.local/bin/trade-control-worker-dev,
# and restarts the systemd user service trade-control-worker-dev (see
# roll_native_worker in deploy-lib.sh). Secrets come from the service's
# EnvironmentFile (~/.config/trade-control/worker-secrets.env), not this script.

set -euo pipefail

ENV_NAME="dev"
ENV_BRANCH="main"
# Dev is the LOCAL native/Postgres worker (127.0.0.1:8787), not Cloudflare
# (which is fully retired — staging is local too). The suffixed `-dev` CLIs
# bake this as their default endpoint so no `--endpoint` flag is needed.
ENV_WEBHOOK="http://127.0.0.1:8787"
ENV_SUFFIX="dev"

source "$(dirname "$0")/deploy-lib.sh"
# 5th arg "native" → skip wrangler deploy (local worker, CLIs only).
deploy_env "$ENV_NAME" "$ENV_BRANCH" "$ENV_WEBHOOK" "$ENV_SUFFIX" native
