#!/usr/bin/env bash
# Vast onstart for SlopTweak (must stay < 4048 chars). Starts the deadman, fetches
# the instance asset bundle pinned by SHA-256, verifies it, and hands off to
# provision.sh.
# Env: SLOPTWEAK_ASSETS_URL, SLOPTWEAK_ASSETS_SHA256, HEARTBEAT_MINUTES;
# CONTAINER_ID, CONTAINER_API_KEY (Vast).
set -euo pipefail
mkdir -p /opt/sloptweak /var/log/sloptweak
cd /opt/sloptweak
exec >>/var/log/sloptweak/onstart.log 2>&1
for v in SLOPTWEAK_ASSETS_URL SLOPTWEAK_ASSETS_SHA256 HEARTBEAT_MINUTES CONTAINER_ID \
  CONTAINER_API_KEY; do
  if [[ -z "${!v:-}" ]]; then
    export "$v=$(tr '\0' '\n' </proc/1/environ | sed -n "s/^$v=//p" | head -n1)"
  fi
done
if [[ -z "$CONTAINER_API_KEY" && -f /root/.vast_api_key ]]; then
  CONTAINER_API_KEY="$(tr -d '\n' </root/.vast_api_key)"
  export CONTAINER_API_KEY
fi

# Deadman. The sidecar's watchdog destroys this instance when the PC stops
# heartbeating, but only while the sidecar runs. If it never starts (asset fetch
# or setup failed) or dies, destroy the instance once it has been unreachable
# for the heartbeat timeout (at least 10 min, so a slow setup isn't killed).
deadman() {
  local mins=${HEARTBEAT_MINUTES:-10} down=0
  [[ "$mins" =~ ^[0-9]+$ ]] || mins=10
  ((mins >= 10)) || mins=10
  while ((down < mins * 60)); do
    sleep 15
    if curl -s -o /dev/null --max-time 5 http://127.0.0.1:8080/__status; then
      down=0
    else
      down=$((down + 15))
    fi
  done
  echo "$(date -u +%FT%TZ) deadman: sidecar unreachable for $mins min; destroying"
  if [[ -z "${CONTAINER_ID:-}" || -z "${CONTAINER_API_KEY:-}" ]]; then
    echo "deadman: CONTAINER_ID/CONTAINER_API_KEY missing; cannot destroy"
    return 1
  fi
  # Header on stdin keeps the key out of the process list.
  until printf 'Authorization: Bearer %s\n' "$CONTAINER_API_KEY" |
    curl -fsS --max-time 30 -X DELETE -H @- -o /dev/null \
      "https://console.vast.ai/api/v0/instances/$CONTAINER_ID/"; do
    sleep 30
  done
  echo "deadman: destroy requested"
}
nohup setsid bash -c "$(declare -f deadman); deadman" >>/var/log/sloptweak/deadman.log 2>&1 &

curl -fsSL --retry 5 -o assets.tar.gz "$SLOPTWEAK_ASSETS_URL"
echo "$SLOPTWEAK_ASSETS_SHA256  assets.tar.gz" | sha256sum -c -
tar -xzf assets.tar.gz
nohup setsid bash /opt/sloptweak/provision.sh >>/var/log/sloptweak/provision.log 2>&1 &
