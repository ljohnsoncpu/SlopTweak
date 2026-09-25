from __future__ import annotations

import json
from collections.abc import Awaitable, Callable
from pathlib import Path
from typing import Any

import pytest
from aiohttp import WSMsgType, web
from aiohttp.test_utils import TestClient, TestServer

import sidecar
from sidecar import SESSION_COOKIE, Config, Sidecar, VastDestroyer, Watchdog, sha256_hex

SECRET = "launcher-secret-for-tests"
BEARER = {"Authorization": f"Bearer {SECRET}"}

ClientFactory = Callable[..., Awaitable[TestClient[Any, Any]]]
ServerFactory = Callable[..., Awaitable[TestServer]]


class FakeClock:
    def __init__(self) -> None:
        self.t = 1000.0

    def __call__(self) -> float:
        return self.t

    def advance(self, seconds: float) -> None:
        self.t += seconds


class FakeInvoke:
    """Stands in for Invoke on 127.0.0.1:9090."""

    def __init__(self) -> None:
        self.queue = {"pending": 0, "in_progress": 0}
        self.seen: list[dict[str, Any]] = []

    def app(self) -> web.Application:
        app = web.Application()
        app.router.add_get("/api/v1/queue/default/status", self.queue_status)
        app.router.add_get("/ws/socket.io/", self.ws_echo)
        app.router.add_route("*", "/{tail:.*}", self.echo)
        return app

    async def queue_status(self, _: web.Request) -> web.Response:
        return web.json_response({"queue": self.queue, "processor": {}})

    async def echo(self, request: web.Request) -> web.Response:
        record = {
            "method": request.method,
            "path": request.rel_url.path_qs,
            "cookie": request.headers.get("Cookie"),
            "authorization": request.headers.get("Authorization"),
            "body": (await request.read()).decode(),
        }
        self.seen.append(record)
        return web.json_response(record, headers={"X-Upstream": "invoke"})

    async def ws_echo(self, request: web.Request) -> web.WebSocketResponse:
        ws = web.WebSocketResponse()
        await ws.prepare(request)
        async for msg in ws:
            if msg.type == WSMsgType.TEXT:
                await ws.send_str(f"echo:{msg.data}")
        return ws


class Harness:
    def __init__(
        self,
        car: Sidecar,
        client: TestClient[Any, Any],
        invoke: FakeInvoke,
        clock: FakeClock,
        destroyed: list[str],
    ) -> None:
        self.car = car
        self.client = client
        self.invoke = invoke
        self.clock = clock
        self.destroyed = destroyed
        # Set explicitly: the Secure cookie wouldn't round-trip over the plain-HTTP test client.
        self.cookie: dict[str, str] = {}

    def set_stage(self, stage: str, **extra: Any) -> None:
        self.car.cfg.status_file.write_text(json.dumps({"stage": stage, **extra}))

    async def login(self) -> None:
        resp = await self.client.post("/__ticket", headers=BEARER)
        ticket = (await resp.json())["ticket"]
        resp = await self.client.get("/__auth", params={"t": ticket}, allow_redirects=False)
        assert resp.status == 302
        self.cookie = {"Cookie": f"{SESSION_COOKIE}={resp.cookies[SESSION_COOKIE].value}"}


@pytest.fixture
async def h(
    tmp_path: Path, aiohttp_client: ClientFactory, aiohttp_server: ServerFactory
) -> Harness:
    invoke = FakeInvoke()
    upstream = await aiohttp_server(invoke.app())
    clock = FakeClock()
    destroyed: list[str] = []

    async def destroyer(reason: str) -> bool:
        destroyed.append(reason)
        return True

    cfg = Config(
        launch_token_hash=sha256_hex(SECRET),
        heartbeat_timeout_s=600,
        idle_timeout_s=1200,
        max_session_s=4 * 3600,
        upstream=str(upstream.make_url("")).rstrip("/"),
        status_file=tmp_path / "status.json",
    )
    car = Sidecar(cfg, clock=clock, destroyer=destroyer)
    client = await aiohttp_client(car.make_app(run_watchdog=False))
    return Harness(car, client, invoke, clock, destroyed)


# ----- config ---------------------------------------------------------------


def test_config_from_env_defaults_and_validation() -> None:
    cfg = Config.from_env({"LAUNCH_TOKEN_HASH": "A" * 64, "IDLE_MINUTES": "5"})
    assert cfg.launch_token_hash == "a" * 64
    assert cfg.idle_timeout_s == 300
    assert cfg.heartbeat_timeout_s == 600
    assert cfg.max_session_s == 4 * 3600
    assert (
        Config.from_env({"LAUNCH_TOKEN_HASH": "a" * 64, "MAX_SESSION_MINUTES": "0"}).max_session_s
        is None
    )
    with pytest.raises(ValueError):
        Config.from_env({"LAUNCH_TOKEN_HASH": "short"})


# ----- auth -----------------------------------------------------------------


async def test_everything_requires_auth(h: Harness) -> None:
    for method, path in [
        ("GET", "/"),
        ("GET", "/api/v1/images/"),
        ("POST", "/api/x"),
        ("GET", "/__status"),
        ("POST", "/__heartbeat"),
        ("POST", "/__ticket"),
    ]:
        resp = await h.client.request(method, path)
        assert resp.status == 401, (method, path)
    assert h.invoke.seen == []


async def test_wrong_bearer_rejected(h: Harness) -> None:
    for header in ["Bearer nope", SECRET, f"Basic {SECRET}", "Bearer "]:
        resp = await h.client.post("/__ticket", headers={"Authorization": header})
        assert resp.status == 401


async def test_ticket_exchange_sets_session_cookie(h: Harness) -> None:
    resp = await h.client.post("/__ticket", headers=BEARER)
    assert resp.status == 200
    body = await resp.json()
    assert body["expires_in"] == 60
    resp = await h.client.get("/__auth", params={"t": body["ticket"]}, allow_redirects=False)
    assert resp.status == 302
    assert resp.headers["Location"] == "/"
    cookie = resp.cookies[SESSION_COOKIE]
    assert cookie["httponly"] and cookie["secure"]
    assert cookie["samesite"] == "Lax"
    resp = await h.client.get("/app", headers={"Cookie": f"{SESSION_COOKIE}={cookie.value}"})
    assert resp.status == 200
    assert (await resp.json())["path"] == "/app"


async def test_ticket_is_single_use(h: Harness) -> None:
    ticket = (await (await h.client.post("/__ticket", headers=BEARER)).json())["ticket"]
    first = await h.client.get("/__auth", params={"t": ticket}, allow_redirects=False)
    second = await h.client.get("/__auth", params={"t": ticket}, allow_redirects=False)
    assert first.status == 302
    assert second.status == 401


async def test_ticket_expires_after_60s(h: Harness) -> None:
    ticket = (await (await h.client.post("/__ticket", headers=BEARER)).json())["ticket"]
    h.clock.advance(61)
    resp = await h.client.get("/__auth", params={"t": ticket}, allow_redirects=False)
    assert resp.status == 401


async def test_bogus_ticket_and_bogus_cookie_rejected(h: Harness) -> None:
    assert (await h.client.get("/__auth", params={"t": "x"})).status == 401
    assert (await h.client.get("/__auth")).status == 401
    resp = await h.client.get("/", headers={"Cookie": f"{SESSION_COOKIE}=forged"})
    assert resp.status == 401


async def test_launch_secret_is_not_a_ticket(h: Harness) -> None:
    resp = await h.client.get("/__auth", params={"t": SECRET}, allow_redirects=False)
    assert resp.status == 401


# ----- proxy ----------------------------------------------------------------


async def test_proxy_strips_session_cookie_and_bearer(h: Harness) -> None:
    await h.login()
    cookie = {"Cookie": f"invoke_pref=dark; {h.cookie['Cookie']}"}
    resp = await h.client.post("/api/v1/thing?x=1", data="payload", headers=cookie)
    body = await resp.json()
    assert resp.headers["X-Upstream"] == "invoke"
    assert body["method"] == "POST"
    assert body["path"] == "/api/v1/thing?x=1"
    assert body["body"] == "payload"
    assert body["cookie"] == "invoke_pref=dark"
    assert body["authorization"] is None

    resp = await h.client.get("/api/v1/images/", headers=BEARER)
    assert resp.status == 200
    assert (await resp.json())["authorization"] is None


async def test_proxy_websocket(h: Harness) -> None:
    await h.login()
    ws = await h.client.ws_connect("/ws/socket.io/", headers=h.cookie)
    await ws.send_str("hello")
    msg = await ws.receive()
    assert msg.type == WSMsgType.TEXT
    assert msg.data == "echo:hello"
    await ws.close()


async def test_websocket_requires_auth(h: Harness) -> None:
    resp = await h.client.get("/ws/socket.io/", headers={"Upgrade": "websocket"})
    assert resp.status == 401


async def test_upstream_down_returns_503(h: Harness) -> None:
    object.__setattr__(h.car.cfg, "upstream", "http://127.0.0.1:9")
    await h.login()
    resp = await h.client.get("/", headers=h.cookie)
    assert resp.status == 503


# ----- heartbeat / status ---------------------------------------------------


async def test_status_reports_stage_and_deadlines(h: Harness) -> None:
    h.set_stage("downloading", progress=0.5)
    h.clock.advance(100)
    body = await (await h.client.get("/__status", headers=BEARER)).json()
    assert body["stage"] == "downloading"
    assert body["progress"] == 0.5
    assert body["destroying"] is None
    assert body["deadlines"]["heartbeat_s"] == 500
    assert body["deadlines"]["idle_s"] is None


async def test_status_without_file_is_booting(h: Harness) -> None:
    body = await (await h.client.get("/__status", headers=BEARER)).json()
    assert body["stage"] == "booting"


async def test_heartbeat_resets_timer(h: Harness) -> None:
    h.clock.advance(590)
    assert (await h.client.post("/__heartbeat", headers=BEARER)).status == 200
    h.clock.advance(590)
    assert await h.car.tick() is None
    h.clock.advance(20)
    assert await h.car.tick() == "heartbeat_lost"


# ----- watchdog (via Sidecar.tick) ------------------------------------------


async def test_heartbeat_loss_destroys_even_while_provisioning(h: Harness) -> None:
    h.set_stage("downloading", progress=0.1)
    h.clock.advance(599)
    assert await h.car.tick() is None
    h.clock.advance(1)
    assert await h.car.tick() == "heartbeat_lost"
    await h.car._destroy_task  # type: ignore[misc]
    assert h.destroyed == ["heartbeat_lost"]
    # Only destroys once.
    h.clock.advance(1000)
    assert await h.car.tick() is None
    assert h.destroyed == ["heartbeat_lost"]
    status = await (await h.client.get("/__status", headers=BEARER)).json()
    assert status["destroying"] == "heartbeat_lost"


async def _keepalive(h: Harness, seconds: float, step: float = 300) -> str | None:
    """Advance time with heartbeats so only idle/max-session can trigger."""
    reason = None
    elapsed = 0.0
    while elapsed < seconds and reason is None:
        h.clock.advance(step)
        elapsed += step
        await h.client.post("/__heartbeat", headers=BEARER)
        reason = await h.car.tick()
    return reason


async def test_idle_only_counts_after_ready(h: Harness) -> None:
    h.set_stage("downloading")
    assert await _keepalive(h, 3000) is None  # 50 min provisioning, never idle
    h.set_stage("ready")
    await h.car.tick()
    assert await _keepalive(h, 1200 - 300) is None
    assert await _keepalive(h, 300) == "idle"


async def test_passive_requests_do_not_reset_idle(h: Harness) -> None:
    h.set_stage("ready")
    await h.car.tick()
    await h.login()
    h.clock.advance(1100)
    await h.client.post("/__heartbeat", headers=BEARER)
    await h.client.get("/api/v1/images/", headers=h.cookie)  # webview GET: passive
    await h.client.post("/api/v1/images/", headers=BEARER)  # launcher traffic: not activity
    h.clock.advance(100)
    await h.client.post("/__heartbeat", headers=BEARER)
    assert await h.car.tick() == "idle"


async def test_session_mutation_resets_idle(h: Harness) -> None:
    h.set_stage("ready")
    await h.car.tick()
    await h.login()
    h.clock.advance(1100)
    await h.client.post("/api/v1/queue/default/enqueue_batch", data="{}", headers=h.cookie)
    h.clock.advance(1100)
    await h.client.post("/__heartbeat", headers=BEARER)
    assert await h.car.tick() is None
    h.clock.advance(100)
    await h.client.post("/__heartbeat", headers=BEARER)
    assert await h.car.tick() == "idle"


async def test_busy_queue_counts_as_activity(h: Harness) -> None:
    h.set_stage("ready")
    await h.car.tick()
    h.invoke.queue = {"pending": 3, "in_progress": 1}
    assert await _keepalive(h, 3000) is None
    h.invoke.queue = {"pending": 0, "in_progress": 0}
    assert await _keepalive(h, 1500) == "idle"


async def test_max_session_destroys(h: Harness) -> None:
    h.set_stage("ready")
    h.invoke.queue = {"pending": 1, "in_progress": 0}  # busy forever
    assert await _keepalive(h, 4 * 3600) == "max_session"


def test_watchdog_unit() -> None:
    clock = FakeClock()
    cfg = Config(
        launch_token_hash="0" * 64, heartbeat_timeout_s=10, idle_timeout_s=5, max_session_s=None
    )
    wd = Watchdog(cfg, clock)
    clock.advance(9)
    assert wd.check() is None
    wd.heartbeat()
    wd.mark_ready()
    clock.advance(5)
    assert wd.check() == "idle"
    wd.activity()
    assert wd.check() is None
    assert wd.deadlines()["max_session_s"] is None


# ----- destroy call ---------------------------------------------------------


async def test_vast_destroyer_calls_delete_with_instance_key(
    aiohttp_server: ServerFactory,
) -> None:
    calls: list[tuple[str, str, str | None]] = []
    responses = [500, 429, 200]

    async def handler(request: web.Request) -> web.Response:
        calls.append((request.method, request.path, request.headers.get("Authorization")))
        status = responses.pop(0)
        return web.json_response({"success": status == 200}, status=status)

    app = web.Application()
    app.router.add_route("*", "/api/v0/instances/{id}/", handler)
    server = await aiohttp_server(app)
    cfg = Config(
        launch_token_hash="0" * 64,
        container_id="52510101",
        container_api_key="instance-key",
        vast_api_base=str(server.make_url("/api/v0")),
    )
    assert await VastDestroyer(cfg, retry_delay_s=0)("idle") is True
    assert calls == [("DELETE", "/api/v0/instances/52510101/", "Bearer instance-key")] * 3


async def test_vast_destroyer_without_credentials_does_not_call() -> None:
    cfg = Config(launch_token_hash="0" * 64)
    assert await VastDestroyer(cfg, retry_delay_s=0)("idle") is False


def test_module_has_no_accidental_print() -> None:
    source = Path(sidecar.__file__).read_text()
    assert "print(" not in source
