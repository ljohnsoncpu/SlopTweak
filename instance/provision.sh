#!/usr/bin/env bash
# SlopTweak instance provisioning. Run by onstart after the asset bundle's SHA-256
# is verified. Order matters: the sidecar and tunnel come up first so the launcher
# can watch progress (and heartbeat) while models download.
#
# Env (from the Vast create call):
#   LAUNCH_TOKEN_HASH   sha256 hex of the launcher's per-session secret (required)
#   MODELS_B64          base64 JSON: [{url, sha256, size_bytes, filename,
#                                      requires_civitai_token}] (required)
#   CIVITAI_TOKEN       user's CivitAI key (only if a model needs it)
#   IDLE_MINUTES, HEARTBEAT_MINUTES, MAX_SESSION_MINUTES   watchdog timers
#   CONTAINER_ID, CONTAINER_API_KEY   injected by Vast
set -Eeuo pipefail

ASSET_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STATE_DIR=/run/sloptweak
LOG_DIR=/var/log/sloptweak
MODELS_DIR=/invokeai/sloptweak-models
SIDECAR_VENV=/opt/sloptweak/sidecar-venv
STATUS_FILE="$STATE_DIR/status.json"
INVOKE_URL=http://127.0.0.1:9090
CLOUDFLARED_VERSION=2026.9.3
CLOUDFLARED_SHA256=77e26d8d900e0b8469f416239d14b5f296525fdf79fee6f511ef55609e3fbac2
CLOUDFLARED_METRICS=127.0.0.1:20241
VAST_API=https://console.vast.ai/api/v0

mkdir -p "$STATE_DIR" "$LOG_DIR" "$MODELS_DIR"
chmod 700 "$STATE_DIR"

log() { printf '%s %s\n' "$(date -u +%FT%TZ)" "$*"; }

# status STAGE [DETAIL] [PROGRESS]. DETAIL must not contain quotes or backslashes.
status() {
  local tmp="$STATUS_FILE.tmp"
  printf '{"stage":"%s","detail":"%s","progress":%s,"updated":%s}\n' \
    "$1" "${2:-}" "${3:-null}" "$(date +%s)" >"$tmp"
  mv "$tmp" "$STATUS_FILE"
  log "status: $1 ${2:-} ${3:-}"
}

fail() {
  status failed "$1"
  exit 1
}
trap 'fail "provision.sh error on line $LINENO"' ERR

# Vast puts CONTAINER_ID/CONTAINER_API_KEY in PID 1's env; fall back to it if our
# parent didn't pass them through.
load_from_pid1() {
  local name value
  for name in "$@"; do
    if [[ -z "${!name:-}" ]]; then
      value="$(tr '\0' '\n' </proc/1/environ | sed -n "s/^${name}=//p" | head -n1)"
      if [[ -n "$value" ]]; then
        export "$name=$value"
      fi
    fi
  done
}
load_from_pid1 CONTAINER_ID CONTAINER_API_KEY LAUNCH_TOKEN_HASH MODELS_B64 CIVITAI_TOKEN \
  IDLE_MINUTES HEARTBEAT_MINUTES MAX_SESSION_MINUTES
if [[ -z "${CONTAINER_API_KEY:-}" && -f /root/.vast_api_key ]]; then
  CONTAINER_API_KEY="$(tr -d '\n' </root/.vast_api_key)"
  export CONTAINER_API_KEY
fi

[[ -n "${LAUNCH_TOKEN_HASH:-}" ]] || fail "LAUNCH_TOKEN_HASH missing"
[[ -n "${MODELS_B64:-}" ]] || fail "MODELS_B64 missing"
[[ -n "${CONTAINER_ID:-}" && -n "${CONTAINER_API_KEY:-}" ]] ||
  log "WARNING: CONTAINER_ID/CONTAINER_API_KEY missing; watchdog cannot self-destroy"

status booting "starting services"

# ---- Invoke (Vast replaces the image entrypoint in ssh runtypes) -------------
export INVOKEAI_ROOT=/invokeai INVOKEAI_HOST=127.0.0.1 INVOKEAI_PORT=9090
export HF_HOME=/invokeai/.cache/huggingface
chown -R ubuntu /invokeai || true
(cd /invokeai && nohup setsid gosu ubuntu invokeai-web >"$LOG_DIR/invokeai.log" 2>&1 &)

# ---- Sidecar (own venv, hashed deps; never touches Invoke's /opt/venv) --------
uv venv -q "$SIDECAR_VENV"
uv pip install -q --python "$SIDECAR_VENV/bin/python" --require-hashes \
  -r "$ASSET_DIR/requirements.txt"
(
  export SLOPTWEAK_STATUS_FILE="$STATUS_FILE" SIDECAR_HOST=127.0.0.1 SIDECAR_PORT=8080
  nohup setsid "$SIDECAR_VENV/bin/python" "$ASSET_DIR/sidecar.py" \
    >"$LOG_DIR/sidecar.log" 2>&1 &
)

# ---- Tunnel ------------------------------------------------------------------
CFD=/opt/sloptweak/cloudflared
curl -fsSL --retry 5 -o "$CFD" \
  "https://github.com/cloudflare/cloudflared/releases/download/${CLOUDFLARED_VERSION}/cloudflared-linux-amd64"
echo "$CLOUDFLARED_SHA256  $CFD" | sha256sum -c --quiet - || fail "cloudflared checksum mismatch"
chmod +x "$CFD"
(nohup setsid "$CFD" tunnel --no-autoupdate --metrics "$CLOUDFLARED_METRICS" \
  --url http://127.0.0.1:8080 >"$LOG_DIR/cloudflared.log" 2>&1 &)

TUNNEL_HOST=""
for _ in $(seq 1 60); do
  TUNNEL_HOST="$(curl -fsS "http://$CLOUDFLARED_METRICS/quicktunnel" 2>/dev/null |
    sed -n 's/.*"hostname":"\([^"]*\)".*/\1/p')" || true
  [[ -n "$TUNNEL_HOST" ]] && break
  sleep 2
done
[[ -n "$TUNNEL_HOST" ]] || fail "tunnel did not come up"
log "tunnel: $TUNNEL_HOST"

# Publish the tunnel host in the instance label; the launcher reads it via the API.
if [[ -n "${CONTAINER_ID:-}" && -n "${CONTAINER_API_KEY:-}" ]]; then
  curl -fsS --retry 5 -X PUT "$VAST_API/instances/$CONTAINER_ID/" \
    -H "Authorization: Bearer $CONTAINER_API_KEY" -H "Content-Type: application/json" \
    -d "{\"label\":\"sloptweak:$TUNNEL_HOST\"}" >/dev/null || fail "could not publish tunnel label"
fi

# ---- Models ------------------------------------------------------------------
# One line per model: url<TAB>sha256<TAB>size_bytes<TAB>filename<TAB>needs_token
MODEL_LINES="$(printf '%s' "$MODELS_B64" | base64 -d | "$SIDECAR_VENV/bin/python" -c '
import json, sys
for m in json.load(sys.stdin):
    print("\t".join([m["url"], m["sha256"].lower(), str(int(m["size_bytes"])),
                     m["filename"], "1" if m.get("requires_civitai_token") else "0"]))
')" || fail "MODELS_B64 is not valid"

TOTAL_BYTES=0
while IFS=$'\t' read -r _ _ size _ _; do
  TOTAL_BYTES=$((TOTAL_BYTES + size))
done <<<"$MODEL_LINES"

bytes_on_disk() {
  local total=0 f
  for f in "$MODELS_DIR"/*; do
    [[ -f "$f" ]] && total=$((total + $(stat -c %s "$f")))
  done
  echo "$total"
}

download_model() {
  local url="$1" sha="$2" size="$3" name="$4" needs_token="$5"
  local dest="$MODELS_DIR/$name" part="$MODELS_DIR/$name.part"
  local -a opts=(-fL --silent --show-error -C - -o "$part")
  if [[ -f "$dest" ]] && [[ "$(stat -c %s "$dest")" == "$size" ]]; then
    log "already present: $name"
    return 0
  fi
  if [[ "$needs_token" == 1 ]]; then
    [[ -n "${CIVITAI_TOKEN:-}" ]] || fail "$name needs a CivitAI key"
  fi
  local attempt pid
  for attempt in 1 2 3 4 5; do
    # curl does not send custom Authorization headers to other hosts on redirect.
    if [[ "$needs_token" == 1 ]]; then
      # Header on stdin: the token never touches disk or the process list.
      printf 'Authorization: Bearer %s\n' "$CIVITAI_TOKEN" |
        curl "${opts[@]}" -H @- "$url" 2>>"$LOG_DIR/download.log" &
    else
      curl "${opts[@]}" "$url" </dev/null 2>>"$LOG_DIR/download.log" &
    fi
    pid=$!
    while kill -0 "$pid" 2>/dev/null; do
      status downloading "$name" "$(awk -v a="$(bytes_on_disk)" -v b="$TOTAL_BYTES" \
        'BEGIN { printf "%.3f", (b > 0 ? a / b : 0) }')"
      sleep 3
    done
    if wait "$pid"; then
      break
    fi
    log "download attempt $attempt failed for $name"
    if [[ "$attempt" == 5 ]]; then
      fail "download failed: $name"
    fi
    sleep $((attempt * 5))
  done
  [[ "$(stat -c %s "$part")" == "$size" ]] || fail "size mismatch: $name"
  status verifying "$name"
  echo "$sha  $part" | sha256sum -c --quiet - || fail "checksum mismatch: $name"
  mv "$part" "$dest"
}

while IFS=$'\t' read -r url sha size name needs_token; do
  download_model "$url" "$sha" "$size" "$name" "$needs_token"
done <<<"$MODEL_LINES"

# ---- Register with Invoke ----------------------------------------------------
status starting "waiting for Invoke"
for _ in $(seq 1 120); do
  curl -fsS "$INVOKE_URL/api/v1/app/version" >/dev/null 2>&1 && break
  sleep 2
done
curl -fsS "$INVOKE_URL/api/v1/app/version" >/dev/null || fail "Invoke did not start"

register_model() {
  local path="$1" resp job_id job_status
  resp="$(curl -sS -X POST -H 'Content-Type: application/json' -d '{}' \
    "$INVOKE_URL/api/v2/models/install?inplace=true&source=$(
      "$SIDECAR_VENV/bin/python" -c 'import sys, urllib.parse; print(urllib.parse.quote(sys.argv[1], safe=""))' "$path"
    )")"
  job_id="$(printf '%s' "$resp" | "$SIDECAR_VENV/bin/python" -c \
    'import json, sys; print(json.load(sys.stdin).get("id", ""))' 2>/dev/null)" || job_id=""
  if [[ -z "$job_id" ]]; then
    # 409 = already registered (e.g. provision re-run); anything else is fatal.
    printf '%s' "$resp" | grep -qi "already" && return 0
    fail "model install rejected: $(basename "$path")"
  fi
  for _ in $(seq 1 300); do
    job_status="$(curl -fsS "$INVOKE_URL/api/v2/models/install/$job_id" |
      "$SIDECAR_VENV/bin/python" -c 'import json, sys; print(json.load(sys.stdin)["status"])')"
    case "$job_status" in
      completed) return 0 ;;
      error | cancelled) fail "model install failed: $(basename "$path")" ;;
    esac
    sleep 2
  done
  fail "model install timed out: $(basename "$path")"
}

status registering "adding models to Invoke"
while IFS=$'\t' read -r _ _ _ name _; do
  register_model "$MODELS_DIR/$name"
done <<<"$MODEL_LINES"

status ready "" 1
