from __future__ import annotations

import base64
import copy
import json
import threading
from collections.abc import Iterator
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any
from urllib.parse import unquote

import pytest

import model_defaults
from model_defaults import apply, find_key, merged, urllib_http, wanted

MODELS_DIR = "/invokeai/sloptweak-models"
MAIN_FIELDS = {
    "vae",
    "vae_precision",
    "scheduler",
    "steps",
    "cfg_scale",
    "cfg_rescale_multiplier",
    "width",
    "height",
    "guidance",
    "cpu_only",
    "fp8_storage",
}


def record(key: str, filename: str, **settings: Any) -> dict[str, Any]:
    full = dict.fromkeys(sorted(MAIN_FIELDS))
    full.update(settings)
    return {
        "key": key,
        "type": "main",
        "path": f"{MODELS_DIR}/{filename}",
        "source": f"{MODELS_DIR}/{filename}",
        "default_settings": full,
    }


class FakeInvoke:
    """Invoke 6.14.1's model store: PATCH replaces default_settings whole."""

    def __init__(self, *records: dict[str, Any]) -> None:
        self.records = {r["key"]: r for r in records}
        self.calls: list[tuple[str, str]] = []

    def __call__(self, method: str, path: str, body: Any) -> Any:
        self.calls.append((method, path))
        if method == "GET" and path == "/api/v2/models/?model_type=main":
            return {"models": copy.deepcopy(list(self.records.values()))}
        key = path.removeprefix("/api/v2/models/i/")
        rec = self.records[key]
        if method == "GET":
            return copy.deepcopy(rec)
        assert method == "PATCH"
        settings = body["default_settings"]
        if not set(settings) <= MAIN_FIELDS:
            raise ValueError("422: extra fields not permitted")
        full = dict.fromkeys(sorted(MAIN_FIELDS))
        full.update(settings)
        rec["default_settings"] = full
        return copy.deepcopy(rec)


def entry(filename: str, **extra: Any) -> dict[str, Any]:
    return {
        "url": "https://x/y",
        "sha256": "a" * 64,
        "size_bytes": 1,
        "filename": filename,
        **extra,
    }


def test_wanted_keeps_only_known_typed_keys() -> None:
    models = [
        entry("a.safetensors", default_settings={"cfg_scale": 1, "steps": 10}),
        entry("vae.safetensors"),
        entry(
            "b.safetensors",
            default_settings={
                "scheduler": "euler_a",
                "steps": "30",
                "cfg_scale": True,
                "vae": "x",
                "cpu_only": True,
            },
        ),
        entry("c.safetensors", default_settings={"nope": 1}),
        entry("d.safetensors", default_settings=["steps", 3]),
    ]
    assert wanted(models) == {
        "a.safetensors": {"cfg_scale": 1, "steps": 10},
        "b.safetensors": {"scheduler": "euler_a"},
    }


def test_find_key_matches_the_file_name() -> None:
    recs = [record("k1", "other.safetensors"), record("k2", "turbo.safetensors")]
    assert find_key(recs, "turbo.safetensors") == "k2"
    assert find_key(recs, "missing.safetensors") is None
    # A file name that is only a suffix of another doesn't match.
    assert find_key(recs, "bo.safetensors") is None


def test_merged_keeps_invokes_own_values() -> None:
    existing = {"width": 1024, "height": 1024, "steps": 35, "cfg_scale": 4.5, "vae": None}
    assert merged(existing, {"cfg_scale": 1, "steps": 10}) == {
        "width": 1024,
        "height": 1024,
        "steps": 10,
        "cfg_scale": 1,
    }
    assert merged(None, {"steps": 3}) == {"steps": 3}


def test_apply_patches_each_model_and_keeps_width() -> None:
    invoke = FakeInvoke(
        record(
            "turbo", "anima-turbo.safetensors", width=1024, height=1024, steps=35, cfg_scale=4.5
        ),
        record("banana", "banana.safetensors", width=1024, height=1024),
    )
    models = [
        entry("anima-turbo.safetensors", default_settings={"cfg_scale": 1, "steps": 10}),
        entry("qwen.safetensors"),
        entry("banana.safetensors", default_settings={"scheduler": "euler_a", "steps": 30}),
    ]
    assert apply(invoke, models) == []
    turbo = invoke.records["turbo"]["default_settings"]
    assert (turbo["cfg_scale"], turbo["steps"], turbo["width"]) == (1, 10, 1024)
    banana = invoke.records["banana"]["default_settings"]
    assert (banana["scheduler"], banana["steps"], banana["height"]) == ("euler_a", 30, 1024)
    assert [m for m, _ in invoke.calls].count("PATCH") == 2


def test_nothing_to_do_makes_no_calls() -> None:
    invoke = FakeInvoke()
    assert apply(invoke, [entry("a.safetensors")]) == []
    assert invoke.calls == []


def test_one_failure_does_not_stop_the_rest() -> None:
    invoke = FakeInvoke(record("b", "b.safetensors"))
    models = [
        entry("missing.safetensors", default_settings={"steps": 5}),
        entry("b.safetensors", default_settings={"steps": 7}),
    ]
    problems = apply(invoke, models)
    assert problems == ["missing.safetensors: not registered"]
    assert invoke.records["b"]["default_settings"]["steps"] == 7


def test_a_patch_that_does_not_stick_is_reported() -> None:
    invoke = FakeInvoke(record("b", "b.safetensors"))

    def forgetful(method: str, path: str, body: Any) -> Any:
        out = invoke(method, path, body)
        if method == "PATCH":
            out["default_settings"]["steps"] = None
        return out

    problems = apply(forgetful, [entry("b.safetensors", default_settings={"steps": 7})])
    assert len(problems) == 1 and "Invoke saved" in problems[0]


@pytest.fixture
def http_invoke() -> Iterator[tuple[str, FakeInvoke]]:
    """FakeInvoke behind a real HTTP server, to exercise urllib_http."""
    fake = FakeInvoke(record("k/1", "m.safetensors", width=832))

    class Handler(BaseHTTPRequestHandler):
        def _serve(self) -> None:
            length = int(self.headers.get("Content-Length") or 0)
            body = json.loads(self.rfile.read(length)) if length else None
            if body is not None:
                assert self.headers["Content-Type"] == "application/json"
            out = json.dumps(fake(self.command, unquote(self.path), body)).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(out)))
            self.end_headers()
            self.wfile.write(out)

        do_GET = _serve
        do_PATCH = _serve

        def log_message(self, *args: Any) -> None:
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_address[1]}", fake
    finally:
        server.shutdown()
        server.server_close()


def test_main_over_http(
    http_invoke: tuple[str, FakeInvoke], monkeypatch: pytest.MonkeyPatch
) -> None:
    url, fake = http_invoke
    models = [entry("m.safetensors", default_settings={"cfg_scale": 5, "steps": 30})]
    monkeypatch.setenv("MODELS_B64", base64.b64encode(json.dumps(models).encode()).decode())
    monkeypatch.setattr(model_defaults, "INVOKE_URL", url)
    monkeypatch.setattr(model_defaults, "urllib_http", lambda: urllib_http(url))
    assert model_defaults.main() == 0
    saved = fake.records["k/1"]["default_settings"]
    # The key is URL-quoted in the path; width set by Invoke survives.
    assert (saved["cfg_scale"], saved["steps"], saved["width"]) == (5, 30, 832)
