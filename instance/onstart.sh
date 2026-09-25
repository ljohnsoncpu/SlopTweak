#!/usr/bin/env bash
# Vast onstart for SlopTweak (must stay < 4048 chars). Fetches the instance asset
# bundle pinned by SHA-256, verifies it, and hands off to provision.sh.
# Env: SLOPTWEAK_ASSETS_URL, SLOPTWEAK_ASSETS_SHA256.
set -euo pipefail
mkdir -p /opt/sloptweak /var/log/sloptweak
cd /opt/sloptweak
exec >>/var/log/sloptweak/onstart.log 2>&1
for v in SLOPTWEAK_ASSETS_URL SLOPTWEAK_ASSETS_SHA256; do
  if [[ -z "${!v:-}" ]]; then
    export "$v=$(tr '\0' '\n' </proc/1/environ | sed -n "s/^$v=//p" | head -n1)"
  fi
done
curl -fsSL --retry 5 -o assets.tar.gz "$SLOPTWEAK_ASSETS_URL"
echo "$SLOPTWEAK_ASSETS_SHA256  assets.tar.gz" | sha256sum -c -
tar -xzf assets.tar.gz
nohup setsid bash /opt/sloptweak/provision.sh >>/var/log/sloptweak/provision.log 2>&1 &
