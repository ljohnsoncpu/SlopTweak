"""SlopTweak instance sidecar.

Sits between the cloudflared tunnel and InvokeAI:

* Auth. The launcher holds a 256-bit secret whose SHA-256 is in the instance env
  (``LAUNCH_TOKEN_HASH``). It authenticates its own calls with
  ``Authorization: Bearer <secret>``. For the webview it mints a single-use ticket
  (``POST /__ticket``, valid 60 s) and opens ``/__auth?t=<ticket>``, which sets an
  HttpOnly session cookie. Everything else gets 401.
* Reverse proxy (HTTP + websockets) to Invoke on 127.0.0.1:9090.
* ``POST /__heartbeat`` and ``GET /__status`` for the launcher.
* Watchdog: destroys this instance through the Vast API using the instance-scoped
  ``CONTAINER_API_KEY`` on heartbeat loss, idle, or max session length.

Idle means no Invoke queue activity and no state-changing (non-GET) requests
from a webview session. GETs, websockets, and launcher (bearer) traffic don't
count, so a window left open doesn't keep the GPU alive.
"""

from __future__ import annotations

import asyncio
import contextlib
import hashlib
import hmac
import json
import logging
import os
import secrets
import time
from collections.abc import Callable, Coroutine, Mapping
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from aiohttp import ClientSession, ClientTimeout, WSMsgType, web

log = logging.getLogger("sloptweak.sidecar")

Clock = Callable[[], float]
Destroyer = Callable[[str], Coroutine[Any, Any, bool]]

SESSION_COOKIE = "st_session"
TICKET_TTL_S = 60.0
MAX_BODY_BYTES = 256 * 1024 * 1024
HOP_HEADERS = frozenset(
    {
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailers",
        "transfer-encoding",
        "upgrade",
        "host",
        "content-length",
    }
)
# Request methods that don't count as user activity for the idle timer.
PASSIVE_METHODS = frozenset({"GET", "HEAD", "OPTIONS"})


def sha256_hex(value: str) -> str:
    return hashlib.sha256(value.encode()).hexdigest()


@dataclass(frozen=True)
class Config:
    launch_token_hash: str
    heartbeat_timeout_s: float = 10 * 60
    idle_timeout_s: float = 20 * 60
    max_session_s: float | None = 4 * 60 * 60
    upstream: str = "http://127.0.0.1:9090"
    status_file: Path = Path("/run/sloptweak/status.json")
    container_id: str = ""
    container_api_key: str = ""
    vast_api_base: str = "https://console.vast.ai/api/v0"
    watchdog_interval_s: float = 15.0

    @classmethod
    def from_env(cls, env: Mapping[str, str]) -> Config:
        token_hash = env.get("LAUNCH_TOKEN_HASH", "").strip().lower()
        if len(token_hash) != 64:
            raise ValueError("LAUNCH_TOKEN_HASH must be a 64-char SHA-256 hex digest")
        max_minutes = float(env.get("MAX_SESSION_MINUTES", "240"))
        return cls(
            launch_token_hash=token_hash,
            heartbeat_timeout_s=float(env.get("HEARTBEAT_MINUTES", "10")) * 60,
            idle_timeout_s=float(env.get("IDLE_MINUTES", "20")) * 60,
            max_session_s=max_minutes * 60 if max_minutes > 0 else None,
            status_file=Path(env.get("SLOPTWEAK_STATUS_FILE", "/run/sloptweak/status.json")),
            container_id=env.get("CONTAINER_ID", ""),
            container_api_key=env.get("CONTAINER_API_KEY", ""),
        )


class Watchdog:
    """Pure timer logic; the caller drives it with an injectable clock."""

    def __init__(self, cfg: Config, clock: Clock) -> None:
        self._cfg = cfg
        self._clock = clock
        now = clock()
        self.started = now
        self.last_heartbeat = now
        self.last_activity = now
        self.ready_since: float | None = None

    def heartbeat(self) -> None:
        self.last_heartbeat = self._clock()

    def activity(self) -> None:
        self.last_activity = self._clock()

    def mark_ready(self) -> None:
        if self.ready_since is None:
            self.ready_since = self._clock()

    def check(self) -> str | None:
        """Return the reason the instance should be destroyed now, or None."""
        now = self._clock()
        if now - self.last_heartbeat >= self._cfg.heartbeat_timeout_s:
            return "heartbeat_lost"
        if self._cfg.max_session_s is not None and now - self.started >= self._cfg.max_session_s:
            return "max_session"
        if self.ready_since is not None:
            idle_from = max(self.last_activity, self.ready_since)
            if now - idle_from >= self._cfg.idle_timeout_s:
                return "idle"
        return None

    def deadlines(self) -> dict[str, float | None]:
        now = self._clock()
        cfg = self._cfg
        idle: float | None = None
        if self.ready_since is not None:
            idle = cfg.idle_timeout_s - (now - max(self.last_activity, self.ready_since))
        return {
            "heartbeat_s": cfg.heartbeat_timeout_s - (now - self.last_heartbeat),
            "idle_s": idle,
            "max_session_s": (
                None if cfg.max_session_s is None else cfg.max_session_s - (now - self.started)
            ),
        }


class Auth:
    def __init__(self, launch_token_hash: str, clock: Clock) -> None:
        self._hash = launch_token_hash
        self._clock = clock
        self._tickets: dict[str, float] = {}  # sha256(ticket) -> expiry
        self._sessions: set[str] = set()

    def check_bearer(self, header: str | None) -> bool:
        if not header or not header.startswith("Bearer "):
            return False
        return hmac.compare_digest(sha256_hex(header[len("Bearer ") :]), self._hash)

    def mint_ticket(self) -> str:
        now = self._clock()
        self._tickets = {h: exp for h, exp in self._tickets.items() if exp > now}
        ticket = secrets.token_urlsafe(32)
        self._tickets[sha256_hex(ticket)] = now + TICKET_TTL_S
        return ticket

    def redeem_ticket(self, ticket: str) -> str | None:
        """Consume a ticket and return a new session id, or None if invalid."""
        expiry = self._tickets.pop(sha256_hex(ticket), None)
        if expiry is None or expiry <= self._clock():
            return None
        session = secrets.token_urlsafe(32)
        self._sessions.add(session)
        return session

    def check_session(self, session: str | None) -> bool:
        if not session:
            return False
        return any(hmac.compare_digest(session, s) for s in self._sessions)


class VastDestroyer:
    """Destroys this instance with its own instance-scoped key."""

    def __init__(self, cfg: Config, retry_delay_s: float = 30.0) -> None:
        self._cfg = cfg
        self._retry_delay_s = retry_delay_s

    async def __call__(self, reason: str) -> bool:
        cfg = self._cfg
        if not cfg.container_id or not cfg.container_api_key:
            log.error("cannot destroy (%s): CONTAINER_ID/CONTAINER_API_KEY missing", reason)
            return False
        url = f"{cfg.vast_api_base}/instances/{cfg.container_id}/"
        headers = {"Authorization": f"Bearer {cfg.container_api_key}"}
        attempt = 0
        async with ClientSession(timeout=ClientTimeout(total=30)) as session:
            while True:
                attempt += 1
                try:
                    async with session.delete(url, headers=headers) as resp:
                        if resp.status == 200:
                            log.warning("destroy requested (%s): ok", reason)
                            return True
                        log.error("destroy (%s) attempt %d: HTTP %d", reason, attempt, resp.status)
                except (TimeoutError, OSError) as exc:
                    log.error("destroy (%s) attempt %d: %s", reason, attempt, exc)
                await asyncio.sleep(self._retry_delay_s)


class Sidecar:
    def __init__(
        self,
        cfg: Config,
        clock: Clock = time.monotonic,
        destroyer: Destroyer | None = None,
    ) -> None:
        self.cfg = cfg
        self.clock = clock
        self.auth = Auth(cfg.launch_token_hash, clock)
        self.watchdog = Watchdog(cfg, clock)
        self.destroyer: Destroyer = destroyer or VastDestroyer(cfg)
        self.destroy_reason: str | None = None
        self._http: ClientSession | None = None
        self._destroy_task: asyncio.Task[bool] | None = None

    # ----- status / watchdog -------------------------------------------------

    def read_stage(self) -> dict[str, Any]:
        try:
            data = json.loads(self.cfg.status_file.read_text())
        except (OSError, ValueError):
            return {"stage": "booting"}
        return data if isinstance(data, dict) else {"stage": "booting"}

    async def _queue_busy(self) -> bool:
        assert self._http is not None
        try:
            async with self._http.get(
                f"{self.cfg.upstream}/api/v1/queue/default/status",
                timeout=ClientTimeout(total=10),
            ) as resp:
                if resp.status != 200:
                    return False
                data = await resp.json()
        except (TimeoutError, OSError, ValueError):
            return False
        queue = data.get("queue", data) if isinstance(data, dict) else {}
        return int(queue.get("pending", 0)) + int(queue.get("in_progress", 0)) > 0

    async def tick(self) -> str | None:
        """One watchdog pass. Returns the destroy reason if one was triggered."""
        if self.destroy_reason is not None:
            return None
        if self.read_stage().get("stage") == "ready":
            self.watchdog.mark_ready()
            if await self._queue_busy():
                self.watchdog.activity()
        reason = self.watchdog.check()
        if reason is not None:
            self.destroy_reason = reason
            log.warning("watchdog: destroying instance (%s)", reason)
            self._destroy_task = asyncio.create_task(self.destroyer(reason))
        return reason

    async def _watchdog_loop(self) -> None:
        while self.destroy_reason is None:
            try:
                await self.tick()
            except Exception:
                log.exception("watchdog tick failed")
            await asyncio.sleep(self.cfg.watchdog_interval_s)

    # ----- handlers ----------------------------------------------------------

    async def handle_ticket(self, request: web.Request) -> web.Response:
        if not self.auth.check_bearer(request.headers.get("Authorization")):
            return web.Response(status=401, text="unauthorized")
        return web.json_response({"ticket": self.auth.mint_ticket(), "expires_in": TICKET_TTL_S})

    async def handle_auth(self, request: web.Request) -> web.StreamResponse:
        session = self.auth.redeem_ticket(request.query.get("t", ""))
        if session is None:
            return web.Response(status=401, text="invalid or expired ticket")
        resp = web.Response(status=302, headers={"Location": "/"})
        resp.set_cookie(SESSION_COOKIE, session, httponly=True, secure=True, samesite="Lax")
        resp.headers["Cache-Control"] = "no-store"
        resp.headers["Referrer-Policy"] = "no-referrer"
        return resp

    async def handle_heartbeat(self, request: web.Request) -> web.Response:
        if not self.auth.check_bearer(request.headers.get("Authorization")):
            return web.Response(status=401, text="unauthorized")
        self.watchdog.heartbeat()
        return web.json_response(self.status_payload())

    async def handle_status(self, request: web.Request) -> web.Response:
        if not self.auth.check_bearer(request.headers.get("Authorization")):
            return web.Response(status=401, text="unauthorized")
        return web.json_response(self.status_payload())

    def status_payload(self) -> dict[str, Any]:
        return {
            **self.read_stage(),
            "destroying": self.destroy_reason,
            "deadlines": self.watchdog.deadlines(),
        }

    async def handle_proxy(self, request: web.Request) -> web.StreamResponse:
        via_session = self.auth.check_session(request.cookies.get(SESSION_COOKIE))
        via_bearer = not via_session and self.auth.check_bearer(
            request.headers.get("Authorization")
        )
        if not (via_session or via_bearer):
            return web.Response(status=401, text="unauthorized")
        if via_session and request.method not in PASSIVE_METHODS:
            self.watchdog.activity()

        headers = self._upstream_headers(request)
        url = self.cfg.upstream + request.rel_url.path_qs
        if request.headers.get("Upgrade", "").lower() == "websocket":
            return await self._proxy_ws(request, url, headers)
        return await self._proxy_http(request, url, headers)

    def _upstream_headers(self, request: web.Request) -> dict[str, str]:
        headers = {
            k: v
            for k, v in request.headers.items()
            if k.lower() not in HOP_HEADERS and k.lower() not in {"authorization", "cookie"}
        }
        # Forward the browser's cookies except ours; Invoke never sees the session.
        other = [f"{k}={v}" for k, v in request.cookies.items() if k != SESSION_COOKIE]
        if other:
            headers["Cookie"] = "; ".join(other)
        return headers

    async def _proxy_http(
        self, request: web.Request, url: str, headers: dict[str, str]
    ) -> web.StreamResponse:
        assert self._http is not None
        body = await request.read()
        try:
            upstream = await self._http.request(
                request.method, url, headers=headers, data=body, allow_redirects=False
            )
        except OSError:
            return web.Response(status=503, text="Invoke is not running yet")
        async with upstream:
            out = web.StreamResponse(
                status=upstream.status,
                headers={k: v for k, v in upstream.headers.items() if k.lower() not in HOP_HEADERS},
            )
            await out.prepare(request)
            async for chunk in upstream.content.iter_chunked(64 * 1024):
                await out.write(chunk)
            await out.write_eof()
            return out

    async def _proxy_ws(
        self, request: web.Request, url: str, headers: dict[str, str]
    ) -> web.StreamResponse:
        assert self._http is not None
        ws_headers = {k: v for k, v in headers.items() if not k.lower().startswith("sec-websocket")}
        try:
            upstream = await self._http.ws_connect(
                url.replace("http", "ws", 1), headers=ws_headers, autoping=True
            )
        except OSError:
            return web.Response(status=503, text="Invoke is not running yet")
        client = web.WebSocketResponse(autoping=True)
        await client.prepare(request)

        async def pump(src: Any, dst: Any) -> None:
            async for msg in src:
                if msg.type == WSMsgType.TEXT:
                    await dst.send_str(msg.data)
                elif msg.type == WSMsgType.BINARY:
                    await dst.send_bytes(msg.data)
                else:
                    break
            await dst.close()

        async with upstream:
            await asyncio.gather(
                pump(client, upstream), pump(upstream, client), return_exceptions=True
            )
        return client

    # ----- app ---------------------------------------------------------------

    def make_app(self, run_watchdog: bool = True) -> web.Application:
        app = web.Application(client_max_size=MAX_BODY_BYTES)
        app.router.add_post("/__ticket", self.handle_ticket)
        app.router.add_get("/__auth", self.handle_auth)
        app.router.add_post("/__heartbeat", self.handle_heartbeat)
        app.router.add_get("/__status", self.handle_status)
        app.router.add_route("*", "/{tail:.*}", self.handle_proxy)

        async def lifecycle(_: web.Application) -> Any:
            self._http = ClientSession(
                timeout=ClientTimeout(total=None, sock_connect=10), auto_decompress=False
            )
            task = asyncio.create_task(self._watchdog_loop()) if run_watchdog else None
            yield
            if task is not None:
                task.cancel()
                with contextlib.suppress(asyncio.CancelledError):
                    await task
            await self._http.close()

        app.cleanup_ctx.append(lifecycle)
        return app


def main() -> None:
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    cfg = Config.from_env(os.environ)
    host = os.environ.get("SIDECAR_HOST", "127.0.0.1")
    port = int(os.environ.get("SIDECAR_PORT", "8080"))
    log.info(
        "sidecar starting on %s:%d (heartbeat %.0fs, idle %.0fs, max %s)",
        host,
        port,
        cfg.heartbeat_timeout_s,
        cfg.idle_timeout_s,
        cfg.max_session_s,
    )
    web.run_app(Sidecar(cfg).make_app(), host=host, port=port, access_log=None)


if __name__ == "__main__":
    main()
