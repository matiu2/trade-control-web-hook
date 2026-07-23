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
#   2. Build the CLIs with TRADE_CONTROL_WEBHOOK set so each webhook-talking
#      binary bakes this environment's URL as its compiled-in default endpoint
#      (build.rs → BAKED_WEBHOOK; see cli/build.rs and tv-arm/build.rs).
#   3. Copy the freshly-built binaries into ~/.cargo/bin under their
#      suffixed names (trade-control-staging, tv-arm-staging, …). The
#      binary is identical bar the baked URL; the suffix is how you pick
#      an environment from the shell.
#   4. Rebuild + install the native worker binary and restart its systemd
#      user service so the deploy rolls the running process, not just the CLIs.
#
# Every env is a LOCAL native/Postgres worker (Cloudflare fully retired). There
# is no `wrangler deploy` — the worker is a long-running local process managed
# by its systemd --user unit.
#
# This is a Cargo *workspace* (root Cargo.toml lists cli/tv-arm/tv-news as
# members), so one `cargo build --release` produces every binary into the
# shared ./target/release/.

set -euo pipefail

# Resolve repo root from this lib's location, regardless of caller cwd.
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CARGO_BIN="${CARGO_HOME:-$HOME/.cargo}/bin"
# Stable install path for the native worker binaries. Each native env gets its
# own suffixed copy (trade-control-worker-dev / -staging) so a rebuild of one
# env never clobbers the code the other env is running. The systemd user units
# (~/.config/systemd/user/trade-control-worker-<suffix>.service) exec these.
WORKER_BIN_DIR="$HOME/.local/bin"

# The CLIs we ship. CLI_PACKAGES are the workspace package names passed to
# `cargo build -p` (note: the cli package is `trade-control-cli` but its
# binary is `trade-control`). CLI_BINARIES are the built artifact names in
# ./target/release/ — each gets the env suffix appended on install
# (e.g. trade-control -> trade-control-staging).
#
# `replay-candles` is a second binary of the `trade-control-cli` package, so it
# builds with that `-p` already; it has no baked webhook (it talks to
# TradingView + the broker via candle-cache, not the worker), so the suffixed
# copy is purely a convenience name — but installing per-env keeps it alongside
# the other dev/staging tools.
CLI_PACKAGES=(trade-control-cli tv-arm tv-news)
CLI_BINARIES=(trade-control tv-arm tv-news replay-candles)

# roll_native_worker <env-name> <suffix>
#
# Build the native worker, install it to the per-env stable path, and restart
# the matching systemd user service so the deploy rolls the running process,
# not just the CLIs.
#
# The service name matches the binary suffix: trade-control-worker-<suffix>.
# If that unit isn't installed on this host (e.g. a fresh checkout that hasn't
# run the systemd setup yet), we install the binary and warn instead of failing
# — the operator can start the worker manually or install the unit.
roll_native_worker() {
  local env_name="$1" suffix="$2"
  local service="trade-control-worker-${suffix}"
  local worker_dest="$WORKER_BIN_DIR/trade-control-worker-${suffix}"

  echo "==> [$env_name] building native worker (trade-control-worker)"
  cargo build --release -p trade-control-worker

  mkdir -p "$WORKER_BIN_DIR"
  cp -f "$REPO_ROOT/target/release/trade-control-worker" "$worker_dest"
  echo "==> [$env_name] installed $worker_dest"

  # Restart the service if its unit is known to the user systemd instance.
  # `list-unit-files` is the reliable "is this unit installed?" probe.
  if systemctl --user list-unit-files "${service}.service" >/dev/null 2>&1 \
     && systemctl --user cat "${service}.service" >/dev/null 2>&1; then
    # Pick up any unit-file edits, then restart onto the fresh binary.
    systemctl --user daemon-reload
    echo "==> [$env_name] restarting ${service}.service"
    systemctl --user restart "${service}.service"
    # Brief settle + health confirmation so a failed roll is loud, not silent.
    sleep 2
    if systemctl --user is-active --quiet "${service}.service"; then
      echo "==> [$env_name] ${service} is active"
    else
      echo "ERROR: ${service} failed to come up after restart." >&2
      echo "       Inspect: journalctl --user -u ${service} -n 40 --no-pager" >&2
      exit 1
    fi
  else
    echo "WARN: systemd user unit '${service}.service' not found — binary installed" >&2
    echo "      but the worker was NOT restarted. Install the unit and enable it:" >&2
    echo "        systemctl --user enable --now ${service}.service" >&2
  fi
}

# deploy_env <env-name> <required-branch> <webhook-url> <suffix>
#
# Every environment is a LOCAL native/Postgres worker (Cloudflare retired). The
# worker is a long-running local process managed by its systemd --user unit, so
# there is nothing to `wrangler deploy`; this bakes+installs the suffixed CLIs
# and rolls the worker service.
deploy_env() {
  local env_name="$1" req_branch="$2" webhook="$3" suffix="$4"

  cd "$REPO_ROOT"

  echo "==> [$env_name] target worker URL: $webhook"

  # 1. Branch guard — each branch owns one environment, so deploying from the
  #    wrong branch would roll the wrong env's worker + CLIs.
  local cur_branch
  cur_branch="$(git rev-parse --abbrev-ref HEAD)"
  if [[ "$cur_branch" != "$req_branch" ]]; then
    echo "ERROR: $env_name deploys from branch '$req_branch', but you are on '$cur_branch'." >&2
    echo "       Run: git checkout $req_branch" >&2
    exit 1
  fi

  echo "==> [$env_name] native/local worker on ${webhook} (managed by its systemd unit)"

  # 2. Build all CLIs with this environment's webhook baked in
  #    (build.rs → BAKED_WEBHOOK).
  echo "==> [$env_name] building CLIs with TRADE_CONTROL_WEBHOOK=$webhook"
  local pkg_args=()
  local pkg
  for pkg in "${CLI_PACKAGES[@]}"; do
    pkg_args+=(-p "$pkg")
  done
  # `TRADE_CONTROL_ENV_SUFFIX` is baked into tv-arm (build.rs → BAKED_ENV_SUFFIX)
  # so `tv-arm-<suffix> ... replay` shells out to the matching
  # `replay-candles-<suffix>` binary (same environment).
  TRADE_CONTROL_WEBHOOK="$webhook" \
  TRADE_CONTROL_ENV_SUFFIX="$suffix" \
    cargo build --release "${pkg_args[@]}"

  # 3. Install suffixed copies into ~/.cargo/bin.
  mkdir -p "$CARGO_BIN"
  local bin dest
  for bin in "${CLI_BINARIES[@]}"; do
    dest="$CARGO_BIN/${bin}-${suffix}"
    cp -f "$REPO_ROOT/target/release/$bin" "$dest"
    echo "==> [$env_name] installed $dest"
  done

  # 4. Rebuild + install the worker binary and restart its systemd user service,
  #    so a deploy rolls the whole env — binary AND running process — not just
  #    the CLIs. The per-suffix binary path + per-suffix unit keep dev and
  #    staging isolated (see WORKER_BIN_DIR above).
  roll_native_worker "$env_name" "$suffix"

  echo "==> [$env_name] done. Shell commands now available:"
  for bin in "${CLI_BINARIES[@]}"; do
    echo "      ${bin}-${suffix}"
  done
  echo "    (Run 'exec zsh' or open a new shell to pick up completions.)"
}
