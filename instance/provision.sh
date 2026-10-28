#!/usr/bin/env bash
# SlopTweak instance provisioning. Run by onstart after the asset bundle's SHA-256
# is verified. Order matters: the sidecar and tunnel come up first so the launcher
# can watch progress (and heartbeat) while models download.
#
# Env (from the Vast create call):
#   LAUNCH_TOKEN_HASH   sha256 hex of the launcher's per-session secret (required)
#   BACKEND             invoke (default), comfyui, or train: the app behind the sidecar
#                       (train = LoRA trainer: train_runner.py, no app UI)
#   MODELS_B64          base64 JSON: [{url, sha256, size_bytes, filename,
#                                      requires_civitai_token,
#                                      default_settings?, dest?}] (required).
#                       dest (comfyui only) is the ComfyUI models subfolder.
#   CIVITAI_TOKEN       user's CivitAI key (only if a model needs it)
#   IDLE_MINUTES, HEARTBEAT_MINUTES, MAX_SESSION_MINUTES   watchdog timers
#   CONTAINER_ID, CONTAINER_API_KEY   injected by Vast
set -Eeuo pipefail

ASSET_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STATE_DIR=/run/sloptweak
LOG_DIR=/var/log/sloptweak
SIDECAR_VENV=/opt/sloptweak/sidecar-venv
STATUS_FILE="$STATE_DIR/status.json"
INVOKE_URL=http://127.0.0.1:9090
COMFY_URL=http://127.0.0.1:8188
COMFY_DIR=/opt/workspace-internal/ComfyUI
COMFY_PYTHON=/venv/main/bin/python
# Identity Edit's ComfyUI nodes, pinned by commit (reviewed in the 2026-10-02 spike).
KREA2EDIT_REPO=https://github.com/lbouaraba/comfyui-krea2edit.git
KREA2EDIT_SHA=86f886dac23013d88996e3a2e99093ba44d322fb
# MOSS-Image runs on ComfyUI's native support (PRs 17095 + 17099), which is newer than
# any release or vastai/comfy image. Only a MOSS model gets this copy, pinned by commit
# and run with the image's python plus an overlay venv; the image's own ComfyUI and
# /venv/main stay untouched.
COMFY_MOSS_REPO=https://github.com/comfyanonymous/ComfyUI.git
COMFY_MOSS_SHA=a639f187a9f6abfecbda9673034f2045148b6d85
COMFY_MOSS_DIR=/opt/sloptweak/ComfyUI-moss
COMFY_MOSS_VENV=/opt/sloptweak/comfy-moss-venv
# Must match the sidecar's SLOPTWEAK_LORA_DIR default.
USER_LORA_DIR=/opt/sloptweak/user-loras
# Catalog file names of the MOSS-Image entry (must match catalog.json and moss.rs).
MOSS_BASE_FILE="moss_image_base_bf16.safetensors"
MOSS_TIME_FILE="moss-image-8-step-dmd-time-embedding.safetensors"
MOSS_TIME_DELTA="moss-image-8-step-dmd-time-delta.safetensors"
CLOUDFLARED_VERSION=2026.9.3
CLOUDFLARED_SHA256=77e26d8d900e0b8469f416239d14b5f296525fdf79fee6f511ef55609e3fbac2
CLOUDFLARED_METRICS=127.0.0.1:20241
VAST_API=https://console.vast.ai/api/v0

mkdir -p "$STATE_DIR" "$LOG_DIR"
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
  IDLE_MINUTES HEARTBEAT_MINUTES MAX_SESSION_MINUTES BACKEND
if [[ -z "${CONTAINER_API_KEY:-}" && -f /root/.vast_api_key ]]; then
  CONTAINER_API_KEY="$(tr -d '\n' </root/.vast_api_key)"
  export CONTAINER_API_KEY
fi

BACKEND="${BACKEND:-invoke}"
export BACKEND
case "$BACKEND" in
  invoke) MODELS_DIR=/invokeai/sloptweak-models ;;
  comfyui) MODELS_DIR=/opt/sloptweak/models ;;
  train) MODELS_DIR=/opt/sloptweak/models ;;
  *) fail "unknown BACKEND: $BACKEND" ;;
esac
mkdir -p "$MODELS_DIR"

[[ -n "${LAUNCH_TOKEN_HASH:-}" ]] || fail "LAUNCH_TOKEN_HASH missing"
[[ -n "${MODELS_B64:-}" ]] || fail "MODELS_B64 missing"
[[ -n "${CONTAINER_ID:-}" && -n "${CONTAINER_API_KEY:-}" ]] ||
  log "WARNING: CONTAINER_ID/CONTAINER_API_KEY missing; watchdog cannot self-destroy"

status booting "starting services"

# ---- Invoke (Vast replaces the image entrypoint in ssh runtypes) -------------
start_invoke() {
  export INVOKEAI_ROOT=/invokeai INVOKEAI_HOST=127.0.0.1 INVOKEAI_PORT=9090
  export HF_HOME=/invokeai/.cache/huggingface
  chown -R ubuntu /invokeai || true
  (cd /invokeai && nohup setsid gosu ubuntu invokeai-web >"$LOG_DIR/invokeai.log" 2>&1 &)
}
if [[ "$BACKEND" == invoke ]]; then
  start_invoke
fi

# ---- Sidecar (own venv, hashed deps; never touches the app's own venv) --------
# uv ships in the Invoke image; the ComfyUI image may not have it, so fall back to
# the system python3's venv + pip (same hash-pinned requirements).
make_sidecar_venv() {
  if command -v uv >/dev/null 2>&1; then
    uv venv -q "$SIDECAR_VENV"
    uv pip install -q --python "$SIDECAR_VENV/bin/python" --require-hashes \
      -r "$ASSET_DIR/requirements.txt"
  else
    python3 -m venv "$SIDECAR_VENV"
    "$SIDECAR_VENV/bin/python" -m pip install -q --require-hashes --no-deps \
      -r "$ASSET_DIR/requirements.txt"
  fi
}
make_sidecar_venv
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
# One line per model: url<TAB>sha256<TAB>size_bytes<TAB>filename<TAB>needs_token<TAB>dest
# (dest is "-" when the catalog entry has none).
MODEL_LINES="$(printf '%s' "$MODELS_B64" | base64 -d | "$SIDECAR_VENV/bin/python" -c '
import json, sys
for m in json.load(sys.stdin):
    print("\t".join([m["url"], m["sha256"].lower(), str(int(m["size_bytes"])),
                     m["filename"], "1" if m.get("requires_civitai_token") else "0",
                     m.get("dest") or "-"]))
')" || fail "MODELS_B64 is not valid"

TOTAL_BYTES=0
while IFS=$'\t' read -r _ _ size _ _ _; do
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

while IFS=$'\t' read -r url sha size name needs_token _; do
  download_model "$url" "$sha" "$size" "$name" "$needs_token"
done <<<"$MODEL_LINES"

# ---- Register with Invoke ----------------------------------------------------
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

finish_invoke() {
  status starting "waiting for Invoke"
  for _ in $(seq 1 120); do
    curl -fsS "$INVOKE_URL/api/v1/app/version" >/dev/null 2>&1 && break
    sleep 2
  done
  curl -fsS "$INVOKE_URL/api/v1/app/version" >/dev/null || fail "Invoke did not start"

  status registering "adding models to Invoke"
  while IFS=$'\t' read -r _ _ _ name _ _; do
    register_model "$MODELS_DIR/$name"
  done <<<"$MODEL_LINES"

  # Catalog-recommended CFG/steps/scheduler into each model's Invoke config.
  # Not fatal: without them the model still works with Invoke's own defaults.
  status registering "applying recommended settings"
  "$SIDECAR_VENV/bin/python" "$ASSET_DIR/model_defaults.py" >>"$LOG_DIR/model-defaults.log" 2>&1 ||
    log "WARNING: some recommended settings were not applied (see model-defaults.log)"
}

# ---- ComfyUI (Identity Edit, MOSS-Image) -------------------------------------
# Models stay in $MODELS_DIR (so a re-run skips finished downloads) and are
# symlinked into ComfyUI's model folders.

# MOSS-Image: a pinned ComfyUI checkout, run by an overlay venv that sees /venv/main's
# packages (torch and friends) but installs ComfyUI's newer requirements on top.
setup_moss_comfy() {
  status registering "installing ComfyUI for MOSS-Image"
  if [[ ! -d "$COMFY_MOSS_DIR/.git" ]]; then
    git clone -q --filter=blob:none "$COMFY_MOSS_REPO" "$COMFY_MOSS_DIR"
  fi
  git -C "$COMFY_MOSS_DIR" checkout -q "$COMFY_MOSS_SHA"
  [[ "$(git -C "$COMFY_MOSS_DIR" rev-parse HEAD)" == "$COMFY_MOSS_SHA" ]] ||
    fail "ComfyUI is not at the pinned commit"
  if [[ ! -x "$COMFY_MOSS_VENV/bin/python" ]]; then
    if command -v uv >/dev/null 2>&1; then
      uv venv -q --python "$COMFY_PYTHON" "$COMFY_MOSS_VENV"
    else
      "$COMFY_PYTHON" -m venv "$COMFY_MOSS_VENV"
    fi
    local main_site moss_site
    main_site="$("$COMFY_PYTHON" -c 'import sysconfig; print(sysconfig.get_paths()["purelib"])')"
    moss_site="$("$COMFY_MOSS_VENV/bin/python" -c 'import sysconfig; print(sysconfig.get_paths()["purelib"])')"
    # A .pth path is searched after the venv's own packages, so upgrades win.
    printf '%s\n' "$main_site" >"$moss_site/sloptweak-main-venv.pth"
  fi
  if command -v uv >/dev/null 2>&1; then
    uv pip install -q --python "$COMFY_MOSS_VENV/bin/python" -r "$COMFY_MOSS_DIR/requirements.txt"
  else
    "$COMFY_MOSS_VENV/bin/python" -m pip install -q -r "$COMFY_MOSS_DIR/requirements.txt"
  fi
}

# The distillation's time-embedding file is not a LoRA: turn it into a patch file
# the stock LoRA loader accepts (see moss_time_delta.py), next to the other models.
build_moss_delta() {
  status registering "preparing the MOSS-Image patch"
  [[ -f "$MODELS_DIR/$MOSS_BASE_FILE" && -f "$MODELS_DIR/$MOSS_TIME_FILE" ]] ||
    fail "MOSS-Image files are missing"
  "$COMFY_MOSS_VENV/bin/python" "$ASSET_DIR/moss_time_delta.py" \
    "$MODELS_DIR/$MOSS_BASE_FILE" "$MODELS_DIR/$MOSS_TIME_FILE" "$MODELS_DIR/$MOSS_TIME_DELTA" \
    >>"$LOG_DIR/moss-delta.log" 2>&1 || fail "could not build the MOSS-Image patch"
}

# Identity Edit: its custom nodes (pinned) and the preloaded workflow.
setup_identity_nodes() {
  local nodes="$COMFY_DIR/custom_nodes"
  status registering "installing the Identity Edit nodes"
  if [[ ! -d "$nodes/comfyui-krea2edit" ]]; then
    git clone -q "$KREA2EDIT_REPO" "$nodes/comfyui-krea2edit"
  fi
  git -C "$nodes/comfyui-krea2edit" checkout -q "$KREA2EDIT_SHA"
  [[ "$(git -C "$nodes/comfyui-krea2edit" rev-parse HEAD)" == "$KREA2EDIT_SHA" ]] ||
    fail "Identity Edit nodes are not at the pinned commit"
  # Raw-UI users start from the working Identity Edit graph (Workflows sidebar).
  mkdir -p "$COMFY_DIR/user/default/workflows"
  # The pack's file names differ from ours, so point its loaders at the files
  # placed above (otherwise the pickers show missing models).
  MODEL_LINES="$MODEL_LINES" "$SIDECAR_VENV/bin/python" - \
    "$nodes/comfyui-krea2edit/workflows/krea2_identity_edit.json" \
    "$COMFY_DIR/user/default/workflows/SlopTweak Identity Edit.json" <<'PY' ||
import json
import os
import sys

loaders = {
    "diffusion_models": "UNETLoader",
    "text_encoders": "CLIPLoader",
    "vae": "VAELoader",
    "loras": "LoraLoaderModelOnly",
}
names = {}
for line in os.environ["MODEL_LINES"].splitlines():
    parts = line.split("\t")
    if len(parts) >= 6 and parts[5] in loaders:
        names[loaders[parts[5]]] = parts[3]
with open(sys.argv[1], encoding="utf-8") as f:
    graph = json.load(f)
for node in graph["nodes"]:
    if node.get("type") in names and node.get("widgets_values"):
        node["widgets_values"][0] = names[node["type"]]
with open(sys.argv[2], "w", encoding="utf-8") as f:
    json.dump(graph, f)
PY
    log "WARNING: could not adapt the preloaded workflow; copying it as is"
  [[ -s "$COMFY_DIR/user/default/workflows/SlopTweak Identity Edit.json" ]] ||
    cp "$nodes/comfyui-krea2edit/workflows/krea2_identity_edit.json" \
      "$COMFY_DIR/user/default/workflows/SlopTweak Identity Edit.json"
}

finish_comfy() {
  local moss=0 run_dir="$COMFY_DIR" run_python="$COMFY_PYTHON" probe=Krea2EditModelPatch
  if grep -qF "$MOSS_TIME_FILE" <<<"$MODEL_LINES"; then
    moss=1
    run_dir="$COMFY_MOSS_DIR"
    run_python="$COMFY_MOSS_VENV/bin/python"
    probe=MOSSImageLayoutPrompt
    setup_moss_comfy
    build_moss_delta
  fi

  local models="$run_dir/models" name dest
  status registering "placing models"
  while IFS=$'\t' read -r _ _ _ name _ dest; do
    case "$dest" in
      diffusion_models | text_encoders | vae | loras) ;;
      *) fail "bad model folder for $name" ;;
    esac
    # The raw time-embedding file is only the delta's source, not a LoRA.
    [[ "$name" == "$MOSS_TIME_FILE" ]] && continue
    mkdir -p "$models/$dest"
    ln -sfn "$MODELS_DIR/$name" "$models/$dest/$name"
  done <<<"$MODEL_LINES"
  if ((moss)); then
    mkdir -p "$models/loras"
    # LoRAs the app uploads for evaluation (sidecar /__lora/) show up as user/<file>.
    mkdir -p "$USER_LORA_DIR"
    ln -sfn "$USER_LORA_DIR" "$models/loras/user"
    ln -sfn "$MODELS_DIR/$MOSS_TIME_DELTA" "$models/loras/$MOSS_TIME_DELTA"
    # Open ComfyUI starts empty otherwise: leave the MOSS workflows in its sidebar.
    MODEL_LINES="$MODEL_LINES" "$SIDECAR_VENV/bin/python" "$ASSET_DIR/moss_workflow.py"       "$run_dir/user/default/workflows" "$MOSS_TIME_DELTA" "$MOSS_TIME_FILE"       >>"$LOG_DIR/moss-workflow.log" 2>&1 ||
      log "WARNING: could not write the MOSS workflows (see moss-workflow.log)"
  else
    setup_identity_nodes
  fi

  # ComfyUI-Manager can install code at runtime; keep it out of the instance.
  local nodes="$run_dir/custom_nodes"
  if [[ -d "$nodes/ComfyUI-Manager" ]]; then
    mkdir -p /opt/sloptweak/disabled-nodes
    mv "$nodes/ComfyUI-Manager" /opt/sloptweak/disabled-nodes/
  fi

  status starting "starting ComfyUI"
  (cd "$run_dir" && nohup setsid "$run_python" main.py --listen 127.0.0.1 --port 8188 --use-pytorch-cross-attention \
    >"$LOG_DIR/comfyui.log" 2>&1 </dev/null &)
  local out=""
  for _ in $(seq 1 180); do
    out="$(curl -fsS "$COMFY_URL/object_info/$probe" 2>/dev/null || true)"
    [[ "$out" == *"$probe"* ]] && break
    sleep 2
  done
  [[ "$out" == *"$probe"* ]] || fail "ComfyUI did not start with the expected nodes"
}

# ---- Trainer (LoRA training; the base model is the one MODELS_B64 entry) -----
# The runner waits for the launcher to upload the job through the sidecar, then
# builds its own pinned environment; nothing here touches the image's Python.
finish_train() {
  local work=/opt/sloptweak/train
  mkdir -p "$work/incoming"
  status starting "starting the trainer"
  (
    export TRAIN_WORK="$work" TRAIN_MODELS_DIR="$MODELS_DIR"
    nohup setsid python3 "$ASSET_DIR/train_runner.py" >"$LOG_DIR/train-runner.log" 2>&1 </dev/null &
  )
}

if [[ "$BACKEND" == comfyui ]]; then
  finish_comfy
elif [[ "$BACKEND" == train ]]; then
  finish_train
else
  finish_invoke
fi

status ready "" 1
