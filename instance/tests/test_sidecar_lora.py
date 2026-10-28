from __future__ import annotations

import hashlib
import json
import struct
from collections.abc import Awaitable, Callable
from pathlib import Path
from typing import Any

import pytest
from aiohttp.test_utils import TestClient

import sidecar
from sidecar import Config, Sidecar, sha256_hex

SECRET = "lora-secret-for-tests"
BEARER = {"Authorization": f"Bearer {SECRET}"}

ClientFactory = Callable[..., Awaitable[TestClient[Any, Any]]]


def safetensors(extra: bytes = b"") -> bytes:
    header = json.dumps({"__metadata__": {}}).encode()
    return struct.pack("<Q", len(header)) + header + extra


class Rig:
    def __init__(self, client: TestClient[Any, Any], lora_dir: Path) -> None:
        self.client = client
        self.dir = lora_dir

    async def put(
        self, name: str, data: bytes, digest: str | None = None, auth: bool = True
    ) -> Any:
        headers = {"X-Sha256": digest or hashlib.sha256(data).hexdigest()}
        if auth:
            headers.update(BEARER)
        return await self.client.put(f"/__lora/{name}", data=data, headers=headers)


async def make_rig(tmp_path: Path, aiohttp_client: ClientFactory, backend: str) -> Rig:
    cfg = Config(
        launch_token_hash=sha256_hex(SECRET),
        backend=backend,
        upstream=sidecar.BACKENDS[backend],
        status_file=tmp_path / "status.json",
        lora_dir=tmp_path / "loras",
    )
    car = Sidecar(cfg, destroyer=None)
    return Rig(await aiohttp_client(car.make_app(run_watchdog=False)), tmp_path / "loras")


@pytest.fixture
async def rig(tmp_path: Path, aiohttp_client: ClientFactory) -> Rig:
    return await make_rig(tmp_path, aiohttp_client, "comfyui")


def test_lora_dir_comes_from_the_environment() -> None:
    cfg = Config.from_env({"LAUNCH_TOKEN_HASH": "a" * 64, "SLOPTWEAK_LORA_DIR": "/x"})
    assert cfg.lora_dir == Path("/x")


async def test_upload_stores_the_file_under_user(rig: Rig) -> None:
    data = safetensors(b"weights")
    r = await rig.put("km-abc.safetensors", data)
    assert r.status == 200
    assert await r.json() == {"name": "user/km-abc.safetensors", "bytes": len(data)}
    assert (rig.dir / "km-abc.safetensors").read_bytes() == data
    assert not list(rig.dir.glob("*.part"))


async def test_needs_the_bearer(rig: Rig) -> None:
    r = await rig.put("a.safetensors", safetensors(), auth=False)
    assert r.status == 401
    assert not rig.dir.exists() or not list(rig.dir.iterdir())


@pytest.mark.parametrize("backend", ["invoke", "train"])
async def test_only_the_comfyui_backend_takes_loras(
    tmp_path: Path, aiohttp_client: ClientFactory, backend: str
) -> None:
    other = await make_rig(tmp_path, aiohttp_client, backend)
    assert (await other.put("a.safetensors", safetensors())).status == 404


@pytest.mark.parametrize(
    "name",
    [
        "a.ckpt",
        "..%2Fa.safetensors",
        "-a.safetensors",
        "a b.safetensors",
        "a" * 100 + ".safetensors",
    ],
)
async def test_bad_names_are_refused(rig: Rig, name: str) -> None:
    assert (await rig.put(name, safetensors())).status in (400, 404)
    assert not rig.dir.exists() or not list(rig.dir.iterdir())


async def test_checksum_mismatch_leaves_nothing(rig: Rig) -> None:
    r = await rig.put("a.safetensors", safetensors(), digest="0" * 64)
    assert r.status == 422
    assert not list(rig.dir.iterdir())


async def test_checksum_header_is_required(rig: Rig) -> None:
    r = await rig.client.put("/__lora/a.safetensors", data=safetensors(), headers=BEARER)
    assert r.status == 400


@pytest.mark.parametrize(
    "data",
    [b"not a model at all", struct.pack("<Q", 10**9) + b"{}", struct.pack("<Q", 2) + b"[]"],
)
async def test_non_safetensors_is_refused(rig: Rig, data: bytes) -> None:
    r = await rig.put("a.safetensors", data)
    assert r.status == 422
    assert not list(rig.dir.iterdir())


async def test_size_cap(rig: Rig, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(sidecar, "LORA_MAX_BYTES", 64)
    r = await rig.put("a.safetensors", safetensors(b"x" * 200))
    assert r.status == 413
    assert not list(rig.dir.iterdir())


def test_default_limits_match_the_app() -> None:
    # moss.rs: MAX_LORAS_PER_GPU = 8 and MAX_LORA_BYTES = 1 GiB.
    assert sidecar.LORA_MAX_FILES == 8
    assert sidecar.LORA_MAX_BYTES == 1024 * 1024 * 1024


async def test_file_count_cap_allows_replacing(rig: Rig, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(sidecar, "LORA_MAX_FILES", 2)
    assert (await rig.put("a.safetensors", safetensors())).status == 200
    assert (await rig.put("b.safetensors", safetensors())).status == 200
    assert (await rig.put("c.safetensors", safetensors())).status == 409
    assert (await rig.put("a.safetensors", safetensors(b"v2"))).status == 200
