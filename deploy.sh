#!/usr/bin/env bash
# Deploy the trade-control-web-hook worker to Cloudflare and (re)install
# the local tv-news CLI so the chart-side tooling moves in lockstep with
# whatever was just deployed.
#
# Order matters: deploy first so a build/test failure aborts before the
# local install side-effects. Both steps are idempotent.

set -euo pipefail

cd "$(dirname "$0")"

echo "==> Deploying worker via wrangler"
wrangler deploy

echo "==> Installing cli CLI from ./cli"
cargo install --path cli

echo "==> Installing tv-news CLI from ./tv-news"
cargo install --path tv-news

echo "==> Installing tv-arm CLI from ./tv-arm"
cargo install --path tv-arm

echo "==> Done"
