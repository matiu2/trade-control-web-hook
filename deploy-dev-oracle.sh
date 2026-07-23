#!/usr/bin/env bash
# Deploy the DEV worker to the Oracle Cloud micro (London AD-1).
#
#   target : REMOTE Oracle E2.1.Micro   152.67.131.141  (branch: main)
#   worker : trade-control-worker as a static musl binary, run by the
#            system systemd unit `trade-control-dev.service` on the micro
#   DB     : local PostgreSQL on the micro (trade_control_dev)
#   health : http://152.67.131.141:8787/health  (firewall-scoped to your IP)
#
# This is DIFFERENT from deploy-dev.sh / deploy-staging.sh: those roll a LOCAL
# systemd --user worker on this desktop. This one cross-compiles a static musl
# binary and ships it to a REMOTE host over SSH, restarting a *system* unit
# there. So it does NOT source deploy-lib.sh — the remote/musl model doesn't fit
# `deploy_env`. It also installs no local CLIs (the -dev CLIs from deploy-dev.sh
# already point at the desktop worker; talk to the Oracle worker with
# `--endpoint http://152.67.131.141:8787`).
#
# ── Why musl? ──────────────────────────────────────────────────────────────
# The build host (Manjaro, glibc 2.35+) is NEWER than Oracle Linux 8 (glibc
# 2.28). A normal dynamic build gets `GLIBC_2.xx not found` on the micro. A
# static musl build has no libc dependency and runs anywhere. This works only
# because sqlx uses tls-rustls (pure Rust) — no OpenSSL C dep to cross-link.
#
# ── Oracle DB feature flags — NOT YET ──────────────────────────────────────
# You asked for "the oracle rust feature flags". They DON'T EXIST YET: the
# worker is hardwired to `sqlx` with the `postgres` feature (worker/Cargo.toml),
# and the Oracle-SQL swappable backend is a spike still being scoped
# (SCOPING-oracle-db-swappable.md / SPIKE-oracle-findings.md). So this deploys
# the Postgres-backed worker against the micro's local Postgres — which is what
# is running there today. When the `oracle` Cargo feature lands, flip
# CARGO_FEATURES below to `--no-default-features --features oracle` and point
# ORACLE_DB_URL at the Autonomous DB (see infra/oracle-db/ORACLE-DB-HANDOFF.md).
# The rest of this script (musl build, ship, restart, health) stays identical.
#
# Prereqs on THIS machine:
#   - rustup target x86_64-unknown-linux-musl  (rustup target add …)
#   - musl-gcc                                  (pacman -S musl / equivalent)
#   - ssh access to opc@$MICRO_IP with ~/.ssh/id_ed25519
#   - signing + admin keys at ~/.config/trade-control/{key,admin-key}.hex
#
# One-time remote bootstrap (Postgres install, DB/role, pg_hba md5, swapfile,
# firewall) is documented in infra/AMD-MICRO-WIN.md. This script assumes that
# has been done and only rolls the worker binary + config + unit.

set -euo pipefail

# ── Config ──────────────────────────────────────────────────────────────────
ENV_BRANCH="main"
MICRO_IP="152.67.131.141"
SSH_USER="opc"
SSH_KEY="$HOME/.ssh/id_ed25519"
REMOTE_DIR="/home/opc/tc"
SERVICE="trade-control-dev.service"
WORKER_PORT="8787"

# musl static target (see "Why musl?" above).
MUSL_TARGET="x86_64-unknown-linux-musl"
# Postgres today; swap to `--no-default-features --features oracle` when it lands.
CARGO_FEATURES=""

# Local secret sources (shipped once; kept 600 on the micro).
SIGNING_KEY_FILE="$HOME/.config/trade-control/key.hex"
ADMIN_KEY_FILE="$HOME/.config/trade-control/admin-key.hex"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SSH_OPTS=(-o StrictHostKeyChecking=accept-new -o BatchMode=yes -o ConnectTimeout=15 -i "$SSH_KEY")
SCP_OPTS=(-o StrictHostKeyChecking=accept-new -o BatchMode=yes -o ConnectTimeout=15 -i "$SSH_KEY")

say() { echo "==> [dev-oracle] $*"; }
die() { echo "ERROR: $*" >&2; exit 1; }

# ── 1. Branch guard (mirror the other deploy scripts) ───────────────────────
cd "$REPO_ROOT"
cur_branch="$(git rev-parse --abbrev-ref HEAD)"
[[ "$cur_branch" == "$ENV_BRANCH" ]] || \
  die "dev-oracle deploys from branch '$ENV_BRANCH', but you are on '$cur_branch'. Run: git checkout $ENV_BRANCH"

# ── 2. Preflight: toolchain + keys + reachability ───────────────────────────
rustup target list --installed 2>/dev/null | grep -qx "$MUSL_TARGET" || \
  die "musl target missing. Run: rustup target add $MUSL_TARGET"
command -v musl-gcc >/dev/null 2>&1 || die "musl-gcc not found. Install the musl toolchain."
[[ -f "$SIGNING_KEY_FILE" ]] || die "signing key not found: $SIGNING_KEY_FILE"
[[ -f "$ADMIN_KEY_FILE"   ]] || die "admin key not found: $ADMIN_KEY_FILE"
say "checking SSH to $SSH_USER@$MICRO_IP"
ssh "${SSH_OPTS[@]}" "$SSH_USER@$MICRO_IP" 'true' >/dev/null 2>&1 || \
  die "cannot SSH to $SSH_USER@$MICRO_IP (firewall scoped to your IP? key loaded?)"

# ── 3. Build the static musl worker ─────────────────────────────────────────
say "building trade-control-worker ($MUSL_TARGET, static)${CARGO_FEATURES:+ [$CARGO_FEATURES]}"
CC_x86_64_unknown_linux_musl=musl-gcc \
  cargo build --release --target "$MUSL_TARGET" -p trade-control-worker $CARGO_FEATURES

BIN="$REPO_ROOT/target/$MUSL_TARGET/release/trade-control-worker"
[[ -x "$BIN" ]] || die "build produced no binary at $BIN"
# Sanity: must be static or it'll GLIBC-fail on the micro. musl release builds
# report "static-pie linked"; a plain static build reports "statically linked".
# A dynamic build (the glibc trap) says "dynamically linked" — that's the reject.
if command -v file >/dev/null 2>&1 && file "$BIN" | grep -q 'dynamically linked'; then
  die "built binary is DYNAMICALLY linked — it will GLIBC-fail on Oracle Linux 8. Check the musl toolchain / CC_${MUSL_TARGET//-/_}=musl-gcc."
fi
say "built $(du -h "$BIN" | cut -f1) static binary"

# ── 4. Stage the worker config (Postgres-on-micro) ──────────────────────────
TMP_CFG="$(mktemp)"
trap 'rm -f "$TMP_CFG"' EXIT
cat > "$TMP_CFG" <<EOF
# Dev worker config — Oracle micro ($MICRO_IP). Managed by deploy-dev-oracle.sh.
# Binds all interfaces; reachable on the public IP (firewall-scoped to your IP).
# DB: local Postgres on the micro. (Swap [database].url to the Autonomous DB
# DSN when the oracle backend feature lands — see infra/oracle-db/.)

[http]
bind_addr = "0.0.0.0"
port      = $WORKER_PORT

[database]
url = "postgresql://tcdev:tcdev@localhost:5432/trade_control_dev"

[scheduler]
engine_secs       = 15
upkeep_secs       = 900
daily_tick_secs   = 900
expiry_sweep_secs = 3600
EOF

# ── 5. Stage the systemd unit (idempotent; MemoryMax guards the 1GB box) ─────
TMP_UNIT="$(mktemp)"
trap 'rm -f "$TMP_CFG" "$TMP_UNIT"' EXIT
cat > "$TMP_UNIT" <<EOF
[Unit]
Description=trade-control dev worker (Oracle micro)
After=network-online.target postgresql.service
Wants=network-online.target

[Service]
Type=simple
User=$SSH_USER
ExecStart=/bin/bash -c 'SIGNING_KEY="\$(tr -d "[:space:]" < $REMOTE_DIR/key.hex)" ADMIN_KEY="\$(tr -d "[:space:]" < $REMOTE_DIR/admin-key.hex)" exec $REMOTE_DIR/trade-control-worker $REMOTE_DIR/dev-worker.toml'
Restart=on-failure
RestartSec=5
# Memory guard: the worker must never OOM-thrash this 1GB micro.
MemoryMax=500M

[Install]
WantedBy=multi-user.target
EOF

# ── 6. Ship binary + config + keys to the micro ─────────────────────────────
say "ensuring $REMOTE_DIR exists on the micro"
ssh "${SSH_OPTS[@]}" "$SSH_USER@$MICRO_IP" "mkdir -p $REMOTE_DIR"

say "shipping worker binary (restart happens after, so a half-copy can't run)"
# Copy to a temp name then atomic-move, so a dropped connection can't leave a
# truncated binary that the service would try to exec.
scp "${SCP_OPTS[@]}" "$BIN"     "$SSH_USER@$MICRO_IP:$REMOTE_DIR/trade-control-worker.new"
scp "${SCP_OPTS[@]}" "$TMP_CFG" "$SSH_USER@$MICRO_IP:$REMOTE_DIR/dev-worker.toml"
say "shipping signing + admin keys"
scp "${SCP_OPTS[@]}" "$SIGNING_KEY_FILE" "$SSH_USER@$MICRO_IP:$REMOTE_DIR/key.hex"
scp "${SCP_OPTS[@]}" "$ADMIN_KEY_FILE"   "$SSH_USER@$MICRO_IP:$REMOTE_DIR/admin-key.hex"
scp "${SCP_OPTS[@]}" "$TMP_UNIT" "$SSH_USER@$MICRO_IP:/tmp/${SERVICE}"

# ── 7. Install unit, swap binary, restart, health-check — all on the micro ──
say "installing unit, swapping binary atomically, restarting $SERVICE"
ssh "${SSH_OPTS[@]}" "$SSH_USER@$MICRO_IP" bash -s <<REMOTE
set -euo pipefail
cd "$REMOTE_DIR"
chmod 600 key.hex admin-key.hex
chmod +x trade-control-worker.new
mv -f trade-control-worker.new trade-control-worker      # atomic swap
sudo mv -f /tmp/${SERVICE} /etc/systemd/system/${SERVICE}
sudo systemctl daemon-reload
sudo systemctl enable ${SERVICE} >/dev/null 2>&1 || true
sudo systemctl restart ${SERVICE}
sleep 3
systemctl is-active --quiet ${SERVICE} || {
  echo "ERROR: ${SERVICE} failed to come up. journalctl -u ${SERVICE} -n 40:" >&2
  sudo journalctl -u ${SERVICE} -n 40 --no-pager >&2
  exit 1
}
# Local health probe (loopback on the micro; independent of the firewall).
curl -fsS --max-time 5 "http://localhost:${WORKER_PORT}/health" >/dev/null \
  && echo "  micro-local /health: ok" \
  || { echo "ERROR: worker active but /health failed" >&2; exit 1; }
REMOTE

# ── 8. Confirm reachability from here (through the firewall) ─────────────────
say "confirming health from this machine (through the OCI + host firewall)"
if curl -fsS --max-time 12 "http://$MICRO_IP:$WORKER_PORT/health" >/dev/null 2>&1; then
  say "OK — http://$MICRO_IP:$WORKER_PORT/health reachable"
else
  echo "WARN: worker is up on the micro but not reachable from here." >&2
  echo "      The OCI security list scopes 8787 to a single IP — if your public" >&2
  echo "      IP changed, re-scope the ingress rule (see infra/AMD-MICRO-WIN.md)." >&2
fi

say "done. Talk to it with:  trade-control-dev --endpoint http://$MICRO_IP:$WORKER_PORT status"
