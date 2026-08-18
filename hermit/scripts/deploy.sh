#!/usr/bin/env bash
# Push the built binary + config + deploy files to the Pi.
#
# Usage:  scripts/deploy.sh <ssh-host> [--restart]
#
# What it does:
#   1. rsync the aarch64 binary   -> /opt/hermit/bin/hermit
#   2. rsync config/               -> /opt/hermit/config/   (never touches core.md
#                                     that consolidation has written under /var/lib)
#   3. rsync deploy/ + scripts/    -> ~/hermit-deploy/       (provision.sh, asound.conf,
#                                     units; provision.sh installs them from there)
#   4. rsync firmware/             -> ~/respeaker-fw/        (for dfu-util)
#   5. --restart: systemctl restart hermit and tail the journal briefly
#
# Assumes provision.sh has already created /opt/hermit and the hermit user.
# Before provisioning, it still works: everything lands in ~/hermit-deploy/ and
# provision.sh copies from there.

set -euo pipefail
cd "$(dirname "$0")/.."

HOST="${1:?usage: scripts/deploy.sh <ssh-host> [--restart]}"
RESTART="${2:-}"
BIN="target/aarch64-unknown-linux-gnu/release/hermit"

if [[ ! -x "$BIN" ]]; then
  echo "error: $BIN not found; run scripts/build-pi.sh first" >&2
  exit 1
fi
if ! file "$BIN" | grep -q "ARM aarch64"; then
  echo "error: $BIN is not an aarch64 binary:" >&2
  file "$BIN" >&2
  exit 1
fi

# macOS ships openrsync, which rejects --info / --no-owner. Probe once and use the
# richest flag set the local rsync actually supports.
if rsync --help 2>&1 | grep -q -- "--info"; then
  RSYNC="rsync -az --info=stats1 --no-owner --no-group"
else
  RSYNC="rsync -az"          # openrsync: portable subset only
fi

echo "== staging bundle in ~/hermit-deploy on $HOST =="
ssh "$HOST" 'mkdir -p ~/hermit-deploy/bin ~/respeaker-fw'
$RSYNC "$BIN"            "$HOST:~/hermit-deploy/bin/hermit"
$RSYNC deploy/ scripts/  "$HOST:~/hermit-deploy/deploy/"
$RSYNC config/           "$HOST:~/hermit-deploy/config/"
$RSYNC firmware/         "$HOST:~/respeaker-fw/"
$RSYNC models/           "$HOST:~/hermit-deploy/models/"
$RSYNC tools/            "$HOST:~/hermit-deploy/tools/"

# If provisioned, install into place. Uses sudo; the hermit user owns /opt/hermit.
if ssh "$HOST" 'test -d /opt/hermit/bin'; then
  echo "== installing to /opt/hermit =="
  ssh "$HOST" 'set -e
    sudo install -o hermit -g hermit -m 0755 ~/hermit-deploy/bin/hermit /opt/hermit/bin/hermit
    sudo cp -a ~/hermit-deploy/config/. /opt/hermit/config/
    sudo install -d -o hermit -g hermit /opt/hermit/models
    sudo cp -a ~/hermit-deploy/models/. /opt/hermit/models/
    sudo chown -R hermit:hermit /opt/hermit/config /opt/hermit/models
    /opt/hermit/bin/hermit --version'
  if [[ "$RESTART" == "--restart" ]]; then
    echo "== restarting hermit.service =="
    ssh "$HOST" 'sudo systemctl restart hermit && sleep 3 && systemctl --no-pager status hermit | head -15 && journalctl -u hermit -n 25 --no-pager'
  fi
else
  echo "== /opt/hermit not present yet: run provision first =="
  echo "   ssh $HOST 'sudo bash ~/hermit-deploy/deploy/provision.sh'"
fi
echo "done."
