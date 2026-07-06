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
# Legacy Pine study title. DEAD PLUMBING: signal detection moved fully into
# Rust (core/src/signals/, evaluated server-side as PinePattern), so tv-arm no
# longer matches a chart study by name and nothing reads BAKED_PINE_NAME. Kept
# only so the deploy_env signature is stable. Dev and staging share this exact
# value in lockstep — see deploy-staging.sh. Set to the canonical source title.
ENV_PINE_NAME="Candle Signals"

source "$(dirname "$0")/deploy-lib.sh"
# 6th arg "native" → skip wrangler deploy (local worker, CLIs only).
deploy_env "$ENV_NAME" "$ENV_BRANCH" "$ENV_WEBHOOK" "$ENV_SUFFIX" "$ENV_PINE_NAME" native
