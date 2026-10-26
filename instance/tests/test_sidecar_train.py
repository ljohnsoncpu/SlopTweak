from __future__ import annotations

import hashlib
import json
import os
from collections.abc import Awaitable, Callable
from pathlib import Path
from typing import Any

import pytest
from aiohttp.test_utils import TestClient

import sidecar
from sidecar import Config, Sidecar, sha256_hex

SECRET = "train-secret-for-tests"
BEARER = {"Authorization": f"Bearer {SECRET}"}

ClientFactory = Callable[..., Awaitable[TestClient[Any, Any]]]


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


class Rig:
    def __init__(self, car: Sidecar, client: TestClient[Any, Any], work: Path) -> None:
        self.car = car
        self.client = client
        self.work = work

    def result(self, rel: str, data: bytes, listed: bool = True) -> None:
        path = self.work / "runtime" / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        if listed:
            manifest = self.work / "manifest.json"
            files = json.loads(manifest.read_text())["files"] if manifest.exists() else []
            files.append({"path": rel, "sha256": sha(data), "bytes": len(data)})
            manifest.write_text(json.dumps({"files": files}))

    async def upload(self, name: str, data: bytes, digest: str | None = None) -> Any:
        return await self.client.put(
            f"/__train/upload/{name}",
            data=data,
            headers={**BEARER, "X-Sha256": digest or sha(data)},
        )


async def make_rig(tmp_path: Path, aiohttp_client: ClientFactory, backend: str = "train") -> Rig:
    work = tmp_path / "train"
    cfg = Config(
        launch_token_hash=sha256_hex(SECRET),
        backend=backend,
        upstream=sidecar.BACKENDS[backend],
        status_file=tmp_path / "status.json",
        train_work=work,
    )
    car = Sidecar(cfg, destroyer=None)
    client = await aiohttp_client(car.make_app(run_watchdog=False))
    return Rig(car, client, work)


@pytest.fixture
async def rig(tmp_path: Path, aiohttp_client: ClientFactory) -> Rig:
    return await make_rig(tmp_path, aiohttp_client)


def test_config_accepts_the_train_backend() -> None:
    cfg = Config.from_env({"LAUNCH_TOKEN_HASH": "a" * 64, "BACKEND": "train", "TRAIN_WORK": "/w"})
    assert cfg.backend == "train" and cfg.train_work == Path("/w")


async def test_every_train_route_needs_the_bearer(rig: Rig) -> None:
    rig.result("checkpoints/a.safetensors", b"w")
    for method, path in [
        ("GET", "/__train/state"),
        ("GET", "/__train/manifest"),
        ("GET", "/__train/log"),
        ("GET", "/__train/file?path=checkpoints/a.safetensors"),
        ("PUT", "/__train/upload/job.json"),
        ("POST", "/__train/start"),
        ("POST", "/__train/cancel"),
    ]:
        resp = await rig.client.request(method, path)
        assert resp.status == 401, path
        resp = await rig.client.request(method, path, headers={"Authorization": "Bearer wrong"})
        assert resp.status == 401, path


async def test_other_backends_do_not_expose_the_train_routes(
    tmp_path: Path, aiohttp_client: ClientFactory
) -> None:
    other = await make_rig(tmp_path, aiohttp_client, backend="invoke")
    resp = await other.client.get("/__train/state", headers=BEARER)
    assert resp.status == 404


async def test_train_backend_has_no_proxy(rig: Rig) -> None:
    resp = await rig.client.get("/anything", headers=BEARER)
    assert resp.status == 404


async def test_state_defaults_to_waiting_then_follows_the_runner(rig: Rig) -> None:
    resp = await rig.client.get("/__train/state", headers=BEARER)
    assert await resp.json() == {
        "stage": "waiting",
        "log_bytes": 0,
        "log_tail": "",
        "started": False,
    }
    rig.work.mkdir(parents=True, exist_ok=True)
    (rig.work / "state.json").write_text(json.dumps({"stage": "train", "step": 5, "total": 9}))
    resp = await rig.client.get("/__train/state", headers=BEARER)
    assert await resp.json() == {"stage": "train", "step": 5, "total": 9, "started": False}


async def test_setup_state_reports_log_size_and_the_newest_tail(rig: Rig) -> None:
    logs = rig.work / "runtime" / "logs"
    logs.mkdir(parents=True)
    (rig.work / "state.json").write_text(
        json.dumps({"stage": "bootstrap", "detail": "installing: packages"})
    )
    old = logs / "bootstrap-venv.log"
    old.write_bytes(b"venv ok\n")
    os.utime(old, (1, 1))
    (logs / "bootstrap-packages.log").write_text("x" * 2000 + "Downloading torch")
    state = await (await rig.client.get("/__train/state", headers=BEARER)).json()
    assert state["log_bytes"] == len("venv ok\n") + 2017
    assert state["log_tail"].endswith("Downloading torch") and len(state["log_tail"]) == 1024


async def test_upload_start_and_cancel_flow(rig: Rig) -> None:
    job, dataset = b'{"job": 1}', b"tar-bytes" * 1000
    resp = await rig.client.post("/__train/start", headers=BEARER)
    assert resp.status == 409, "nothing uploaded yet"
    assert (await rig.upload("job.json", job)).status == 200
    resp = await rig.client.post("/__train/start", headers=BEARER)
    assert resp.status == 409, "dataset still missing"
    resp = await rig.upload("dataset.tar", dataset)
    assert resp.status == 200 and (await resp.json())["bytes"] == len(dataset)
    assert (rig.work / "incoming" / "dataset.tar").read_bytes() == dataset
    assert not list((rig.work / "incoming").glob("*.part"))

    resp = await rig.client.post("/__train/start", headers=BEARER)
    assert resp.status == 200
    assert (rig.work / "incoming" / "start").exists()
    state = await (await rig.client.get("/__train/state", headers=BEARER)).json()
    assert state["started"] is True
    # The job can't be swapped after the run begins.
    assert (await rig.upload("job.json", b"{}")).status == 409
    assert (rig.work / "incoming" / "job.json").read_bytes() == job

    resp = await rig.client.post("/__train/cancel", headers=BEARER)
    assert resp.status == 200 and (rig.work / "incoming" / "cancel").exists()


async def test_upload_rejects_bad_requests(rig: Rig) -> None:
    assert (await rig.upload("evil.sh", b"x")).status == 404
    assert (await rig.upload("..%2Fjob.json", b"x")).status == 404
    resp = await rig.client.put("/__train/upload/job.json", data=b"x", headers=BEARER)
    assert resp.status == 400, "X-Sha256 is required"
    resp = await rig.upload("job.json", b"payload", digest="0" * 64)
    assert resp.status == 422
    assert not (rig.work / "incoming" / "job.json").exists()
    assert not list((rig.work / "incoming").glob("*.part"))


async def test_upload_size_limit(rig: Rig, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(sidecar, "TRAIN_UPLOAD_MAX_BYTES", 1024)
    resp = await rig.upload("dataset.tar", b"x" * 4096)
    assert resp.status == 413
    assert not (rig.work / "incoming" / "dataset.tar").exists()
    assert not list((rig.work / "incoming").glob("*.part"))


async def test_files_are_served_only_from_the_manifest(rig: Rig) -> None:
    rig.result("checkpoints/lora.safetensors", b"weights")
    rig.result("secret.txt", b"not listed", listed=False)
    (rig.work / "incoming").mkdir(parents=True)
    (rig.work / "incoming" / "job.json").write_text("{}")

    resp = await rig.client.get(
        "/__train/file", params={"path": "checkpoints/lora.safetensors"}, headers=BEARER
    )
    assert resp.status == 200
    assert await resp.read() == b"weights"
    assert resp.headers["X-Sha256"] == sha(b"weights")

    for path in ["secret.txt", "../incoming/job.json", "/etc/passwd", "", "checkpoints"]:
        resp = await rig.client.get("/__train/file", params={"path": path}, headers=BEARER)
        assert resp.status == 404, path


async def test_a_manifest_entry_cannot_escape_the_runtime_dir(rig: Rig) -> None:
    (rig.work / "incoming").mkdir(parents=True)
    (rig.work / "incoming" / "job.json").write_text("top secret")
    (rig.work / "runtime").mkdir()
    (rig.work / "manifest.json").write_text(
        json.dumps({"files": [{"path": "../incoming/job.json", "sha256": "0" * 64, "bytes": 1}]})
    )
    resp = await rig.client.get(
        "/__train/file", params={"path": "../incoming/job.json"}, headers=BEARER
    )
    assert resp.status == 404


async def test_manifest_and_log_endpoints(rig: Rig) -> None:
    resp = await rig.client.get("/__train/manifest", headers=BEARER)
    assert await resp.json() == {"files": []}
    rig.result("loss.csv", b"step,loss\n")
    listing = await (await rig.client.get("/__train/manifest", headers=BEARER)).json()
    assert [f["path"] for f in listing["files"]] == ["loss.csv"]

    resp = await rig.client.get("/__train/log", headers=BEARER)
    assert resp.status == 200 and await resp.text() == ""
    rig.result("logs/train.log", b"A" * 40_000 + b"tail-end", listed=False)
    resp = await rig.client.get("/__train/log", params={"name": "train"}, headers=BEARER)
    body = await resp.text()
    assert len(body) == sidecar.TRAIN_LOG_TAIL_BYTES and body.endswith("tail-end")
    resp = await rig.client.get("/__train/log", params={"name": "../state"}, headers=BEARER)
    assert resp.status == 400


@pytest.mark.parametrize(
    ("stage", "busy"),
    [("bootstrap", True), ("train", True), ("evaluate", True), ("waiting", False), ("done", False)],
)
async def test_watchdog_sees_training_as_activity(rig: Rig, stage: str, busy: bool) -> None:
    rig.work.mkdir(parents=True, exist_ok=True)
    (rig.work / "state.json").write_text(json.dumps({"stage": stage}))
    assert await rig.car._queue_busy() is busy
