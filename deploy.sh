#!/usr/bin/env bash
# Deprecated. Replaced by per-environment deploy scripts that bake the
# correct webhook URL into the CLIs and install them under suffixed names:
#
#   ./deploy-dev.sh       # main   branch -> trade-control-web-hook        + *-dev CLIs
#   ./deploy-staging.sh   # staging branch -> trade-control-web-hook-staging + *-staging CLIs
#   ./deploy-live.sh      # (added at first prod promotion)
#
# Pick the one matching your current branch. See DEPLOYED.md for the
# branch -> environment model.

set -euo pipefail
echo "deploy.sh is deprecated — use ./deploy-dev.sh or ./deploy-staging.sh." >&2
echo "(They bake the per-env webhook into the CLIs and install suffixed names.)" >&2
exit 1
