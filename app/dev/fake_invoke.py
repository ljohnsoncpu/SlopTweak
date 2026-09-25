"""A tiny stand-in for InvokeAI's HTTP API, for the $0 dev checks.

Serves just what SlopTweak touches, with the shapes of Invoke 6.14.1
(findings §4):

* GET  /                                      a page titled FAKE INVOKE
* GET  /api/v1/images/?is_intermediate=&categories=&offset=&limit=   newest first
* GET  /api/v1/images/i/<name>[/full|/thumbnail]
* POST /api/v1/images/upload?image_category=&is_intermediate=      multipart "file"
* GET  /api/v1/queue/default/status
* GET  /api/v1/queue/default/item_ids?order_dir=DESC
* GET  /api/v1/queue/default/i/<id>   with session.prepared_source_mapping + results

Like the real thing, an image's `node_id` is the *prepared* node id (a uuid);
only the queue item maps it back to its source node (`canvas_output:*`).

Test control (called directly on this port, never through the sidecar):

* POST /__fake/generate?gallery=N&canvas=M   N Generate-tab images (gallery)
  and M Canvas runs (a staged intermediate + a scratch intermediate), each
  one completed queue item
* GET  /__fake/state                                   what's stored

Stdlib only. Binds 127.0.0.1.

    python dev/fake_invoke.py [port]
"""

from __future__ import annotations

import json
import sys
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any
from urllib.parse import parse_qs, urlparse

# A 1x1 PNG.
PNG = bytes.fromhex(
    "89504e470d0a1a0a0000000d4948445200000001000000010806000000"
    "1f15c4890000000d49444154789c6300010000050001"
    "0d0a2db40000000049454e44ae426082"
)
PAGE = b"<!doctype html><title>FAKE INVOKE</title><h1>fake invoke</h1>"

lock = threading.Lock()
images: list[dict[str, Any]] = []  # oldest first
files: dict[str, bytes] = {}
queue = {"pending": 0, "in_progress": 0, "completed": 0, "failed": 0}
queue_items: list[dict[str, Any]] = []  # oldest first


def add(category: str, intermediate: bool, data: bytes = PNG) -> dict[str, Any]:
    name = f"{uuid.uuid4()}.png"
    img = {
        "image_name": name,
        "image_category": category,
        "is_intermediate": intermediate,
        "node_id": str(uuid.uuid4()),
        "created_at": time.strftime("%Y-%m-%d %H:%M:%S.000", time.gmtime()),
        "width": 1,
        "height": 1,
    }
    images.append(img)
    files[name] = data
    return img


def run(output: dict[str, Any], scratch: dict[str, Any] | None) -> None:
    """One completed queue item whose `canvas_output` node made `output`."""
    mapping = {output["node_id"]: f"canvas_output:{uuid.uuid4().hex[:10]}"}
    results: dict[str, Any] = {
        output["node_id"]: {
            "type": "image_output",
            "image": {"image_name": output["image_name"]},
        }
    }
    if scratch is not None:
        mapping[scratch["node_id"]] = f"l2i:{uuid.uuid4().hex[:10]}"
        results[scratch["node_id"]] = {
            "type": "image_output",
            "image": {"image_name": scratch["image_name"]},
        }
    queue_items.append(
        {
            "item_id": len(queue_items) + 1,
            "status": "completed",
            "queue_id": "default",
            "session": {"prepared_source_mapping": mapping, "results": results},
        }
    )
    queue["completed"] += 1


class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt: str, *args: Any) -> None:
        pass

    def send(self, code: int, body: bytes, ctype: str = "application/json") -> None:
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def json(self, obj: Any, code: int = 200) -> None:
        self.send(code, json.dumps(obj).encode())

    def do_GET(self) -> None:
        u = urlparse(self.path)
        q = parse_qs(u.query)
        path = u.path
        if path in ("/", "/index.html"):
            return self.send(200, PAGE, "text/html")
        if path == "/api/v1/images/":
            want_inter = q.get("is_intermediate", [None])[0]
            cats = q.get("categories")
            offset = int(q.get("offset", ["0"])[0])
            limit = int(q.get("limit", ["10"])[0])
            with lock:
                sel = [
                    i
                    for i in reversed(images)
                    if (
                        want_inter is None
                        or str(i["is_intermediate"]).lower() == want_inter
                    )
                    and (not cats or i["image_category"] in cats)
                ]
            return self.json(
                {
                    "items": sel[offset : offset + limit],
                    "offset": offset,
                    "limit": limit,
                    "total": len(sel),
                }
            )
        if path.startswith("/api/v1/images/i/"):
            parts = path[len("/api/v1/images/i/") :].split("/")
            with lock:
                data = files.get(parts[0])
                meta = next((i for i in images if i["image_name"] == parts[0]), None)
            if data is None or meta is None:
                return self.json({"detail": "Image not found"}, 404)
            if len(parts) == 1:
                return self.json(meta)
            return self.send(
                200, data, "image/png" if data.startswith(b"\x89PNG") else "image/jpeg"
            )
        if path == "/api/v1/queue/default/item_ids":
            with lock:
                ids = [q["item_id"] for q in reversed(queue_items)]
            return self.json({"item_ids": ids, "total_count": len(ids)})
        if path.startswith("/api/v1/queue/default/i/"):
            want = path.rsplit("/", 1)[-1]
            with lock:
                item = next((q for q in queue_items if str(q["item_id"]) == want), None)
            if item is None:
                return self.json({"detail": "not found"}, 404)
            return self.json(item)
        if path == "/api/v1/queue/default/status":
            with lock:
                return self.json({"queue": dict(queue, queue_id="default")})
        if path == "/__fake/state":
            with lock:
                return self.json({"images": images, "queue": queue})
        return self.json({"detail": "Not Found"}, 404)

    def do_POST(self) -> None:
        u = urlparse(self.path)
        q = parse_qs(u.query)
        body = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        if u.path == "/api/v1/images/upload":
            # Good enough multipart parsing for one file: the part body sits
            # between the first blank line and the closing boundary.
            ctype = self.headers.get("Content-Type", "")
            boundary = ctype.split("boundary=")[-1].encode()
            start = body.find(b"\r\n\r\n") + 4
            end = body.rfind(b"\r\n--" + boundary)
            data = body[start:end] if start > 3 and end > start else b""
            if not data:
                return self.json({"detail": "no file"}, 422)
            with lock:
                img = add(
                    q.get("image_category", ["user"])[0],
                    q.get("is_intermediate", ["false"])[0] == "true",
                    data,
                )
            return self.json(img, 201)
        if u.path == "/__fake/generate":
            n = {k: int(q.get(k, ["0"])[0]) for k in ("gallery", "canvas")}
            with lock:
                for _ in range(n["canvas"]):
                    run(add("general", True), add("general", True))
                for _ in range(n["gallery"]):
                    run(add("general", False), None)
            return self.json(n)
        return self.json({"detail": "Not Found"}, 404)


def main() -> None:
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 9090
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()


if __name__ == "__main__":
    main()
