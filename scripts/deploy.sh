#!/usr/bin/env bash
#
# Build the aarch64 .deb on THIS (x86_64) host via Docker and install it on the
# production server. Same target host as ratzek-services-http-backend, but this
# project ships a Debian package instead of a bare binary: the systemd unit,
# config directory and enable-on-install logic come from the package (cargo-deb).
#
# Why a .deb: dpkg owns the files, tracks the version, installs the systemd unit
# and its maintainer scripts, and makes upgrade/removal clean.
#
# Usage:  scripts/deploy.sh [ssh-host]      (default host: ratzek)
# Prereqs: docker + cargo-deb on this host; root SSH to the target (as the
#          reference deploy assumes — apt/dpkg/systemctl run without sudo).
set -euo pipefail

IMAGE=daly-bms-exporter-cross-aarch64
TARGET=aarch64-unknown-linux-gnu
PROFILE=deploy
PKG=daly-bms-exporter
SERVICE=daly-bms-exporter
CONFIG_DIR=/etc/daly-bms-exporter
MAX_GLIBC=2.31                       # production (Debian 11) glibc ceiling
REMOTE="${1:-ratzek}"

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"
SSH=(ssh -o LogLevel=ERROR "$REMOTE")

# 1) Cross-compile + package into a .deb (Makefile handles the Docker build).
echo ">> building $PKG .deb for $TARGET…"
make deb

DEB="$(ls -t "target/debian/${PKG}"_*_arm64.deb 2>/dev/null | head -1)"
[ -f "$DEB" ] || { echo "!! no .deb produced under target/debian/" >&2; exit 1; }

# 2) Safety net: verify the packaged binary won't out-require the Pi's glibc
#    (same check as the reference deploy — the .deb doesn't enforce glibc).
BINARY="target/$TARGET/$PROFILE/$PKG"
NEED="$(docker run --rm -v "$REPO":/src -w /src "$IMAGE" \
  bash -c "aarch64-linux-gnu-objdump -T '$BINARY' | grep -oE 'GLIBC_[0-9.]+' | sort -uV | tail -1" \
  | sed 's/GLIBC_//')"
echo ">> max glibc required: ${NEED:-none} (ceiling $MAX_GLIBC)"
if [ -n "$NEED" ] && [ "$(printf '%s\n%s\n' "$MAX_GLIBC" "$NEED" | sort -V | tail -1)" != "$MAX_GLIBC" ]; then
  echo "!! binary needs glibc $NEED > $MAX_GLIBC — would not run on the Pi. Aborting." >&2
  exit 1
fi
echo ">> package: $DEB ($(du -h "$DEB" | cut -f1))"

# 3) Ship the .deb and install it on prod.
echo ">> shipping to $REMOTE…"
scp -o LogLevel=ERROR "$DEB" "$REMOTE:/tmp/"
REMOTE_DEB="/tmp/$(basename "$DEB")"
"${SSH[@]}" "set -e
  # apt resolves Depends (libc6); fall back to dpkg + fix-up if apt is too old.
  apt-get install -y '$REMOTE_DEB' || { dpkg -i '$REMOTE_DEB'; apt-get -f install -y; }
  rm -f '$REMOTE_DEB'
  # The package ships only config.example.yaml; seed config.yaml on first deploy
  # so the service can start (edit it afterwards for the real listen address).
  if [ ! -f $CONFIG_DIR/config.yaml ]; then
    cp $CONFIG_DIR/config.example.yaml $CONFIG_DIR/config.yaml
    echo 'seeded $CONFIG_DIR/config.yaml from example — review it'
  fi
  # The unit is enabled but not auto-started/restarted by the package, so do it
  # here explicitly (works for both first install and upgrade).
  systemctl restart $SERVICE
  sleep 3
  echo \"deployed: \$($PKG --version 2>/dev/null || echo '?')\"
  echo \"service: \$(systemctl is-active $SERVICE)\""
echo ">> done."
