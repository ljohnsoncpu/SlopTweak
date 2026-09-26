"""Write the catalog's recommended settings into Invoke's model configs.

Run by provision.sh after the models are registered. Each MODELS_B64 entry may
carry ``default_settings`` (cfg_scale, steps, scheduler); they go into that
model's config via ``PATCH /api/v2/models/i/{key}`` (Invoke 6.14.1). That call
replaces ``default_settings`` as a whole, so the values Invoke set itself at
registration (e.g. width/height) are read first and kept.

Invoke's UI applies these when the user clicks "Use default settings" by the
model picker; it doesn't apply them on model select.

Stdlib only, so it runs with the sidecar venv's Python. Exit status 1 means at
least one model kept Invoke's own defaults; the session still works.
"""

from __future__ import annotations

import base64
import json
import os
import sys
import urllib.parse
import urllib.request
from collections.abc import Callable
from pathlib import PurePosixPath
from typing import Any

INVOKE_URL = "http://127.0.0.1:9090"
TIMEOUT_S = 30

# method, path, JSON body (or None) -> parsed JSON response
Http = Callable[[str, str, Any], Any]

# What the instance passes on, whatever MODELS_B64 says. Invoke's
# MainModelDefaultSettings forbids unknown keys.
_TYPES: dict[str, tuple[type, ...]] = {
    "cfg_scale": (int, float),
    "steps": (int,),
    "scheduler": (str,),
}


def wanted(models: list[dict[str, Any]]) -> dict[str, dict[str, Any]]:
    """File name -> the settings to apply, for entries that have any."""
    out: dict[str, dict[str, Any]] = {}
    for m in models:
        raw = m.get("default_settings")
        if not isinstance(raw, dict):
            continue
        clean = {
            k: v
            for k, v in raw.items()
            if k in _TYPES and isinstance(v, _TYPES[k]) and not isinstance(v, bool)
        }
        if clean:
            out[str(m["filename"])] = clean
    return out


def find_key(records: list[dict[str, Any]], filename: str) -> str | None:
    """The key of the registered main model whose file is ``filename``."""
    for r in records:
        for field in ("path", "source"):
            value = r.get(field)
            if isinstance(value, str) and PurePosixPath(value).name == filename:
                key = r.get("key")
                return key if isinstance(key, str) else None
    return None


def merged(existing: Any, want: dict[str, Any]) -> dict[str, Any]:
    """Invoke's current settings (nulls dropped) overlaid with ours."""
    base = existing if isinstance(existing, dict) else {}
    out = {k: v for k, v in base.items() if v is not None}
    out.update(want)
    return out


def apply(http: Http, models: list[dict[str, Any]]) -> list[str]:
    """Apply every entry's settings. Returns one line per model that failed."""
    todo = wanted(models)
    if not todo:
        return []
    records = http("GET", "/api/v2/models/?model_type=main", None).get("models", [])
    problems = []
    for filename, want in todo.items():
        try:
            key = find_key(records, filename)
            if key is None:
                raise LookupError("not registered")
            path = f"/api/v2/models/i/{urllib.parse.quote(key, safe='')}"
            current = http("GET", path, None)
            body = {"default_settings": merged(current.get("default_settings"), want)}
            saved = http("PATCH", path, body).get("default_settings") or {}
            if any(saved.get(k) != v for k, v in want.items()):
                raise ValueError(f"Invoke saved {saved}")
            print(f"{filename}: default settings {want}")
        except Exception as e:  # one bad model shouldn't stop the others
            problems.append(f"{filename}: {e}")
    return problems


def urllib_http(base: str = INVOKE_URL) -> Http:
    def call(method: str, path: str, body: Any) -> Any:
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(base + path, data=data, method=method)  # noqa: S310
        if data is not None:
            req.add_header("Content-Type", "application/json")
        # base is Invoke on 127.0.0.1; never a user-controlled URL.
        with urllib.request.urlopen(req, timeout=TIMEOUT_S) as resp:  # noqa: S310
            return json.load(resp)

    return call


def main() -> int:
    models = json.loads(base64.b64decode(os.environ["MODELS_B64"]))
    problems = apply(urllib_http(), models)
    for p in problems:
        print(f"WARNING: {p}", file=sys.stderr)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
