#!/usr/bin/env bash
# Shared deploy machinery for the trade-control environments.
#
# Sourced by deploy-dev.sh / deploy-staging.sh / (later) deploy-live.sh.
# Each wrapper sets ENV_NAME, ENV_BRANCH, ENV_WEBHOOK and the suffix, then
# calls `deploy_env`. Keeping the URLs in the wrappers (one place each)
# means next week's "web-hook becomes prod, cut a new web-hook-dev" remap
# is a one-line edit per script, not a hunt through shared logic.
#
# What it does, in order (deploy first so a build failure aborts before any
# local install side-effects — every step is idempotent):
#   1. Assert we're on the branch that owns this environment (guards
#      against deploying staging code to the dev worker, or vice versa).
#   2. `wrangler deploy` the worker (wrangler.toml on the branch already
#      points at the right worker name / KV / R2).
#   3. Build the three CLIs with TRADE_CONTROL_WEBHOOK set so each binary
#      bakes this environment's URL as its compiled-in default endpoint
#      (build.rs → BAKED_WEBHOOK; see cli/build.rs and tv-arm/build.rs).
#   4. Copy the freshly-built binaries into ~/.cargo/bin under their
#      suffixed names (trade-control-staging, tv-arm-staging, …). The
#      binary is identical bar the baked URL; the suffix is how you pick
#      an environment from the shell.
#
# This is a Cargo *workspace* (root Cargo.toml lists cli/tv-arm/tv-news as
# members), so one `cargo build --release` produces all three into the
# shared ./target/release/.

set -euo pipefail

# Resolve repo root from this lib's location, regardless of caller cwd.
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CARGO_BIN="${CARGO_HOME:-$HOME/.cargo}/bin"

# The CLIs we ship. CLI_PACKAGES are the workspace package names passed to
# `cargo build -p` (note: the cli package is `trade-control-cli` but its
# binary is `trade-control`). CLI_BINARIES are the built artifact names in
# ./target/release/ — each gets the env suffix appended on install
# (e.g. trade-control -> trade-control-staging).
CLI_PACKAGES=(trade-control-cli tv-arm tv-news)
CLI_BINARIES=(trade-control tv-arm tv-news)

# deploy_env <env-name> <required-branch> <webhook-url> <suffix> <pine-name>
#
# <pine-name> is the Pine study title this environment's tv-arm arms
# against (baked via TRADE_CONTROL_PINE_NAME → BAKED_PINE_NAME). Lets each
# environment pin a distinct Pine version living as a separate study on the
# same chart (e.g. "Candle Signals v24" vs "Candle Signals v25"). Optional —
# defaults to the canonical "Candle Signals" when empty/unset.
deploy_env() {
  local env_name="$1" req_branch="$2" webhook="$3" suffix="$4" pine_name="${5:-Candle Signals}"

  cd "$REPO_ROOT"

  echo "==> [$env_name] target worker URL: $webhook"

  # 1. Branch guard. The branch carries the wrangler.toml that names the
  #    worker, so deploying from the wrong branch would hit the wrong env.
  local cur_branch
  cur_branch="$(git rev-parse --abbrev-ref HEAD)"
  if [[ "$cur_branch" != "$req_branch" ]]; then
    echo "ERROR: $env_name deploys from branch '$req_branch', but you are on '$cur_branch'." >&2
    echo "       Run: git checkout $req_branch" >&2
    exit 1
  fi

  # Sanity: the branch's wrangler.toml worker name should match the URL host.
  local worker_name expected_host
  worker_name="$(grep -E '^name *= *"' wrangler.toml | head -1 | sed -E 's/^name *= *"([^"]+)".*/\1/')"
  expected_host="${webhook#https://}"; expected_host="${expected_host%%.*}"
  if [[ "$worker_name" != "$expected_host" ]]; then
    echo "WARN: wrangler.toml worker name '$worker_name' != webhook host '$expected_host'." >&2
    echo "      Deploying anyway, but double-check the URL in deploy-$suffix.sh." >&2
  fi

  # 2. Deploy the worker.
  echo "==> [$env_name] wrangler deploy"
  wrangler deploy

  # 3. Build all CLIs with this environment's webhook + Pine study name
  #    baked in (build.rs → BAKED_WEBHOOK / BAKED_PINE_NAME).
  echo "==> [$env_name] building CLIs with TRADE_CONTROL_WEBHOOK=$webhook"
  echo "==> [$env_name] Pine study target: $pine_name"
  local pkg_args=()
  local pkg
  for pkg in "${CLI_PACKAGES[@]}"; do
    pkg_args+=(-p "$pkg")
  done
  TRADE_CONTROL_WEBHOOK="$webhook" TRADE_CONTROL_PINE_NAME="$pine_name" \
    cargo build --release "${pkg_args[@]}"

  # 4. Install suffixed copies into ~/.cargo/bin.
  mkdir -p "$CARGO_BIN"
  local bin dest
  for bin in "${CLI_BINARIES[@]}"; do
    dest="$CARGO_BIN/${bin}-${suffix}"
    cp -f "$REPO_ROOT/target/release/$bin" "$dest"
    echo "==> [$env_name] installed $dest"
  done

  echo "==> [$env_name] done. Shell commands now available:"
  for bin in "${CLI_BINARIES[@]}"; do
    echo "      ${bin}-${suffix}"
  done
  echo "    (Run 'exec zsh' or open a new shell to pick up completions.)"
}
