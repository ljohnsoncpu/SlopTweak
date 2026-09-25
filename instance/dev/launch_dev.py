"""Drive a SlopTweak instance by hand (Phase 1). Stdlib only.

    python dev/launch_dev.py offers
    python dev/launch_dev.py create --offer ID --assets-url URL --assets-sha SHA [--model ...]
    python dev/launch_dev.py watch          # waits for ready, prints login URL, heartbeats
    python dev/launch_dev.py ticket         # fresh one-time login URL
    python dev/launch_dev.py status
    python dev/launch_dev.py destroy

VAST_API_KEY and CIVITAI_TOKEN come from the process env or, on Windows, the
user env (HKCU\\Environment). The per-instance launch secret is kept in
.dev/state.json (gitignored) so separate commands can share it; it dies with
the instance.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import secrets
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
INSTANCE_DIR = HERE.parent
STATE_FILE = INSTANCE_DIR / ".dev" / "state.json"
CATALOG = INSTANCE_DIR.parent / "catalog" / "catalog.json"
VAST = "https://console.vast.ai/api/v0"
IMAGE = (
    "ghcr.io/invoke-ai/invokeai:v6.14.1-cuda"
    "@sha256:39a7e3b182c4646634d62cf3ebdefd2082573ae20cdba773f9703fee29e600dd"
)
UA = {"User-Agent": "SlopTweak-dev/0.1"}
DEV_MODELS: dict[str, dict[str, Any]] = {
    "sd15": {
        "url": "https://huggingface.co/stable-diffusion-v1-5/stable-diffusion-v1-5/resolve/main/v1-5-pruned-emaonly.safetensors",
        "sha256": "6ce0161689b3853acaa03779ec93eafe75a02f4ced659bee03f50797806fa2fa",
        "size_bytes": 4265146304,
        "filename": "v1-5-pruned-emaonly.safetensors",
        "requires_civitai_token": False,
    }
}
# Dev only: the official Invoke image breaks sshd's permission checks (see findings).
SSH_FIX = (
    "fixssh(){ chown root:root /root /root/.ssh /root/.ssh/authorized_keys; "
    "chmod 700 /root/.ssh; chmod 600 /root/.ssh/authorized_keys; } 2>/dev/null; "
    "fixssh; (for i in $(seq 1 24); do sleep 5; fixssh; done) &\n"
)


def secret_env(name: str) -> str:
    value = os.environ.get(name, "")
    if not value and sys.platform == "win32":
        import winreg

        try:
            with winreg.OpenKey(winreg.HKEY_CURRENT_USER, "Environment") as key:
                value = str(winreg.QueryValueEx(key, name)[0])
        except OSError:
            value = ""
    return value.strip()


def http(
    method: str,
    url: str,
    token: str | None = None,
    body: Any = None,
    timeout: float = 30,
) -> tuple[int, Any]:
    headers = dict(UA)
    if token:
        headers["Authorization"] = f"Bearer {token}"
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            raw = resp.read()
            status = resp.status
    except urllib.error.HTTPError as exc:
        raw = exc.read()
        status = exc.code
    try:
        return status, json.loads(raw) if raw else None
    except ValueError:
        return status, raw.decode(errors="replace")[:300]


def vast(method: str, path: str, body: Any = None) -> Any:
    key = secret_env("VAST_API_KEY")
    if not key:
        raise SystemExit("VAST_API_KEY is not set")
    status, data = http(method, f"{VAST}{path}", key, body)
    if status >= 400:
        raise SystemExit(f"Vast {method} {path}: HTTP {status} {data}")
    return data


def load_state() -> dict[str, Any]:
    if not STATE_FILE.exists():
        raise SystemExit("no dev instance recorded; run create first")
    state: dict[str, Any] = json.loads(STATE_FILE.read_text())
    return state


def save_state(state: dict[str, Any]) -> None:
    STATE_FILE.parent.mkdir(exist_ok=True)
    STATE_FILE.write_text(json.dumps(state, indent=2))


def model_entry(name: str) -> dict[str, Any]:
    if name in DEV_MODELS:
        return DEV_MODELS[name]
    for model in json.loads(CATALOG.read_text())["models"]:
        if model["id"] == name:
            f = model["files"][0]
            return {k: f[k] for k in ("url", "sha256", "size_bytes", "filename")} | {
                "requires_civitai_token": f.get("requires_civitai_token", False)
            }
    raise SystemExit(f"unknown model {name!r}")


def instance(iid: int) -> dict[str, Any]:
    data = vast("GET", f"/instances/{iid}/")
    inst: dict[str, Any] = data.get("instances") or {}
    return inst


def tunnel_host(inst: dict[str, Any]) -> str | None:
    label = inst.get("label") or ""
    return label.split(":", 1)[1] if label.startswith("sloptweak:") else None


# ----- commands ---------------------------------------------------------------


def cmd_offers(_: argparse.Namespace) -> None:
    query = {
        "verified": {"eq": True},
        "rentable": {"eq": True},
        "rented": {"eq": False},
        "reliability": {"gte": 0.98},
        "inet_down": {"gte": 1000},
        "gpu_ram": {"gte": 12000},
        "num_gpus": {"eq": 1},
        "disk_space": {"gte": 50},
        "cuda_max_good": {"gte": 12.4},
        "type": "ondemand",
        "order": [["dph_total", "asc"]],
        "limit": 6,
    }
    for o in vast("POST", "/bundles/", query)["offers"]:
        print(
            f"{o['id']:>10}  {o['gpu_name']:<14} {o['gpu_ram'] / 1024:>4.0f} GB  "
            f"${o['dph_total']:.3f}/hr  rel {o.get('reliability2') or o['reliability']:.3f}  "
            f"{o['inet_down']:>6.0f} Mbps  {o.get('geolocation')}"
        )


def cmd_create(args: argparse.Namespace) -> None:
    if STATE_FILE.exists():
        raise SystemExit(f"{STATE_FILE} exists; destroy the previous instance first")
    model = model_entry(args.model)
    civitai = secret_env("CIVITAI_TOKEN") if model["requires_civitai_token"] else ""
    if model["requires_civitai_token"] and not civitai:
        raise SystemExit("model needs CIVITAI_TOKEN")
    launch_secret = secrets.token_urlsafe(32)
    env = {
        "LAUNCH_TOKEN_HASH": hashlib.sha256(launch_secret.encode()).hexdigest(),
        "MODELS_B64": base64.b64encode(json.dumps([model]).encode()).decode(),
        "IDLE_MINUTES": str(args.idle),
        "HEARTBEAT_MINUTES": str(args.heartbeat),
        "MAX_SESSION_MINUTES": str(args.max_session),
        "SLOPTWEAK_ASSETS_URL": args.assets_url,
        "SLOPTWEAK_ASSETS_SHA256": args.assets_sha,
    }
    if civitai:
        env["CIVITAI_TOKEN"] = civitai
    onstart = (INSTANCE_DIR / "onstart.sh").read_text().replace("\r\n", "\n")
    if args.ssh:
        onstart = SSH_FIX + onstart
    body = {
        "image": IMAGE,
        "disk": args.disk,
        "runtype": "ssh_direct",
        "label": "sloptweak",
        "env": " ".join(f"-e {k}={v}" for k, v in env.items()),
        "onstart": onstart,
        "cancel_unavail": True,
    }
    resp = vast("PUT", f"/asks/{args.offer}/", body)
    iid = int(resp["new_contract"])
    save_state({"instance_id": iid, "secret": launch_secret, "created": time.time()})
    print(f"created instance {iid} (offer {args.offer}, model {args.model})")


def sidecar(state: dict[str, Any], host: str, method: str, path: str) -> tuple[int, Any]:
    try:
        return http(method, f"https://{host}{path}", state["secret"], timeout=15)
    except (urllib.error.URLError, OSError) as exc:
        return 0, str(exc)


def login_url(state: dict[str, Any], host: str) -> str | None:
    status, data = sidecar(state, host, "POST", "/__ticket")
    if status != 200:
        print(f"ticket failed: {status} {data}")
        return None
    return f"https://{host}/__auth?t={data['ticket']}"


def cmd_watch(args: argparse.Namespace) -> None:
    state = load_state()
    iid = state["instance_id"]
    t0 = state["created"]
    last_line = ""
    host: str | None = None
    ready_announced = False
    while True:
        inst = instance(iid)
        if not inst:
            print(f"+{time.time() - t0:.0f}s instance {iid} is gone")
            return
        host = host or tunnel_host(inst)
        line = (
            f"vast={inst.get('actual_status')} msg={(inst.get('status_msg') or '').strip()[:60]!r}"
        )
        if "Error response from daemon" in line:
            print(f"image error: {line}")
            return
        if host:
            status, data = sidecar(state, host, "POST", "/__heartbeat")
            if status == 200:
                line = (
                    f"stage={data.get('stage')} detail={data.get('detail')!r} "
                    f"progress={data.get('progress')} destroying={data.get('destroying')} "
                    f"deadlines={ {k: v and round(v) for k, v in data['deadlines'].items()} }"
                )
                if data.get("stage") == "ready" and not ready_announced:
                    ready_announced = True
                    print(f"+{time.time() - t0:.0f}s READY  tunnel https://{host}")
                    url = login_url(state, host)
                    if url:
                        print(f"login (single use, 60 s): {url}")
                    if args.exit_when_ready:
                        return
                if data.get("stage") == "failed":
                    print(f"+{time.time() - t0:.0f}s FAILED: {data.get('detail')}")
                    return
            else:
                line = f"tunnel={host} sidecar HTTP {status}"
        if line != last_line:
            print(f"+{time.time() - t0:.0f}s {line}", flush=True)
            last_line = line
        time.sleep(args.interval)


def cmd_ticket(_: argparse.Namespace) -> None:
    state = load_state()
    host = tunnel_host(instance(state["instance_id"]))
    if not host:
        raise SystemExit("tunnel not published yet")
    print(login_url(state, host))


def cmd_status(_: argparse.Namespace) -> None:
    state = load_state()
    inst = instance(state["instance_id"])
    if not inst:
        print("instance is gone")
        return
    print(
        f"instance {state['instance_id']}: {inst.get('actual_status')} "
        f"${inst.get('dph_total', 0):.4f}/hr label={inst.get('label')!r}"
    )
    host = tunnel_host(inst)
    if host:
        print(sidecar(state, host, "GET", "/__status"))


def cmd_destroy(_: argparse.Namespace) -> None:
    state = load_state()
    iid = state["instance_id"]
    if instance(iid):
        print(vast("DELETE", f"/instances/{iid}/"))
    else:
        print(f"instance {iid} already gone")
    STATE_FILE.unlink()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    sub = parser.add_subparsers(required=True)
    sub.add_parser("offers").set_defaults(fn=cmd_offers)
    c = sub.add_parser("create")
    c.add_argument("--offer", type=int, required=True)
    c.add_argument("--assets-url", required=True)
    c.add_argument("--assets-sha", required=True)
    c.add_argument("--model", default="sd15")
    c.add_argument("--disk", type=int, default=50)
    c.add_argument("--idle", type=float, default=20)
    c.add_argument("--heartbeat", type=float, default=10)
    c.add_argument("--max-session", type=float, default=240)
    c.add_argument("--ssh", action="store_true", help="dev: make SSH work on the Invoke image")
    c.set_defaults(fn=cmd_create)
    w = sub.add_parser("watch")
    w.add_argument("--interval", type=float, default=20)
    w.add_argument("--exit-when-ready", action="store_true")
    w.set_defaults(fn=cmd_watch)
    sub.add_parser("ticket").set_defaults(fn=cmd_ticket)
    sub.add_parser("status").set_defaults(fn=cmd_status)
    sub.add_parser("destroy").set_defaults(fn=cmd_destroy)
    args = parser.parse_args()
    args.fn(args)


if __name__ == "__main__":
    main()
