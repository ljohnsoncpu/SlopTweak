# Phase 0 Findings

Status: **complete (2026-09-24).** Docs pass plus a live Vast rental.
Items 1–5 are verified live. Item 6 (Tauri) is verified from docs only, and
its $0 local check is deferred to the start of Phase 2.

Markers:
- ✅ confirmed (docs/source, or **Live** = observed on a real rental)
- ⚠️ differs from docs or PLAN.md assumptions; PLAN.md itself not edited
- 🧪 still unverified

Versions: Vast REST API v0, InvokeAI **v6.14.1** (2026-09-06), cloudflared
**2026.9.3**, tauri **2.11.6**.

---

## 1. Vast REST API ✅

| Need | Call |
| --- | --- |
| Base URL | `https://console.vast.ai/api/v0` |
| Auth | `Authorization: Bearer <key>`. Account keys are 64 hex chars. A 32-char value fails with `auth_error: Invalid user key`. |
| Offer search | `POST /bundles/`, JSON filters with `eq/neq/gt/gte/lt/lte/in/notin`. Fields used live: `verified`, `rentable`, `rented`, `reliability`, `inet_down` (Mbps), `gpu_ram` (**MB**), `num_gpus`, `disk_space`, `direct_port_count`, `cuda_max_good`, `type:"ondemand"`. Sort `order: [["dph_total","asc"]]`, `limit`. Offer id = `id`. ⚠️ **Live:** filtering by `{"id":{"eq":N}}` returned no offers for an offer that *was* available. Don't use it to re-check one offer; re-run the full search, or just attempt the create. |
| Create | `PUT /asks/{offer_id}/`. Body: `image`, `disk` (GB), `runtype`, `env` (**one Docker-flag string**: `"-e K=V -p 8080:8080"`), `onstart` (**≤ 4048 chars**), `label`, `cancel_unavail`. ✅ **Live response: `{success, new_contract, instance_api_key}`** (`new_contract` = instance id; `instance_api_key` is 64 chars and missing from the reference page). |
| Status | `GET /instances/{id}/` → object under `instances`: `actual_status` (`null` → `loading` → `running`), `cur_state`, `status_msg`, `public_ipaddr`, `ports` (`{"8080/tcp":[{"HostPort":"40268"}]}`), `ssh_host/ssh_port`, `label`. |
| $/hr | `dph_total` on the instance. **Live:** it includes storage, so it runs slightly above the offer's price ($0.1068 vs $0.095). |
| Destroy | `DELETE /instances/{id}/` → `{"success": true}`. **Live:** gone from `GET /instances/` within ≤ 7 s. |
| Balance | `GET /users/current`. ⚠️ **Live: `balance` was `0`; the prepaid amount was in `credit` ($11.79).** Wizard/cost bar must read `credit` (🧪 what `balance` means when owed). |
| Label | `PUT /instances/{id}/` `{"label": ...}` → `{"success": true}` (live, including from the instance's own key). |
| Container logs | `PUT /instances/request_logs/{id}/` `{"tail":"200"}` → `temp_download_url` (S3), readable a few seconds later. Works without SSH, so it's good for diagnostics. |

⚠️ **Bad image = silent hang.** With a nonexistent tag, `status_msg` showed
`Error response from daemon: manifest unknown` while `actual_status` stayed
`loading` for 10+ min (and `cur_state` flipped to `stopped`). Phase 2 must
treat `status_msg` matching `Error response from daemon` as an immediate
failure and destroy.

⚠️ **Dead-host hang (Phase 1, instance 52513014, offer 37862807):**
`actual_status` stayed `loading` for 21 min with an empty `status_msg`, while
`intended_status` and `cur_state` were `stopped` and the logs said
`No such container`. The host stopped it before the image ran. It cost
~$0.035 before we destroyed it. → Phase 2: treat
`actual_status=loading` + `intended_status=stopped` as failed and move to
the next offer, and cap `loading` at ~8 min regardless.

## 2. Instance self-identity and scoped keys ✅ Live

- `CONTAINER_ID` and `CONTAINER_API_KEY` exist in **PID 1's environment**
  (so onstart and its children see them). They are **not** in SSH sessions.
  The same key is also written to `/root/.vast_api_key`, and
  `/root/.vast_containerlabel` = `C.<id>`.
- The instance key equals the create response's `instance_api_key`.
- **Scope, tested from inside instance 52510101:**

  | Action | Result |
  | --- | --- |
  | `GET /instances/<self>/` | 200 |
  | `PUT /instances/<self>/` label | 200 |
  | `DELETE /instances/<self>/` | **200, instance gone in ≤ 7 s** |
  | `GET /users/current/`, `GET /auth/apikeys/` | 401 "lacks the user_read permission group" |
  | `DELETE` / `PUT` on another instance id | 401 "lacks proper privileges … due to a constraint on id" |
  | `PUT /asks/0/` (create) | 401 "lacks the api.ask route access" |
  | `POST /auth/apikeys/` | 401 "lacks the user_write permission group" |
  | `GET /instances/`, `POST /bundles/` | 200. Harmless: the list was filtered or only this instance existed (🧪 which, since only one instance was running); offer search is public data. |

  → A host that reads this key can at most stop, destroy, or relabel **that
  one instance**. It can't see the account, spend money, or mint keys.
- Scoped user keys also exist (`POST /auth/apikeys`, permissions with
  `constraints: {"id": {"eq": N}}`), but that fallback **is no longer
  needed** and wasn't tested live.

## 3. Official InvokeAI image ✅ Live

- ⚠️ **Tags are `vX.Y.Z-cuda|-rocm|-cpu`**, not the docs' `vX.Y.Z`. Pin:
  `ghcr.io/invoke-ai/invokeai:v6.14.1-cuda@sha256:39a7e3b182c4646634d62cf3ebdefd2082573ae20cdba773f9703fee29e600dd`.
  Never pin below 6.13.8 (security fixes). `latest` = `main-cuda`.
- Layout: venv `/opt/venv` (Python 3.12, `invokeai-web` on PATH), source
  `/opt/invokeai`, `INVOKEAI_ROOT=/invokeai`, `gosu` present, and **`uv` at
  `/usr/bin/uv`**. No `ss`/`netstat`.
- ⚠️ The image defaults to `INVOKEAI_HOST=0.0.0.0`. ✅ **Live:** exporting
  `INVOKEAI_HOST=127.0.0.1` in onstart gives
  `Invoke running on http://127.0.0.1:9090`. The raw public port
  (`-p 8080`) only ever reaches the sidecar.
- ✅ **Live: onstart in `ssh_direct` can start Invoke itself.** Vast replaces
  the image entrypoint in `ssh*` runtypes, so onstart must replicate it:
  `export INVOKEAI_ROOT=/invokeai INVOKEAI_HOST=127.0.0.1 INVOKEAI_PORT=9090 HF_HOME=/invokeai/.cache/huggingface; mkdir -p /invokeai; chown -R ubuntu /invokeai; cd /invokeai; nohup gosu ubuntu invokeai-web &`.
  Invoke is ready ~18 s after onstart begins (first boot, empty DB).
- **Timing (RTX A4000 host, 6.5 Gbps):** cold create → `running` **245 s**
  (~120 s pull, then ~60 s while Vast builds its SSH wrapper layer). A warm
  host with the image cached took **41 s**. Budget 4–5 min on a cold host
  before provisioning starts; favour offers where the image is cached if
  the API exposes that (🧪).
- ⚠️ **SSH into this image fails by default** (`bad ownership or modes for
  file /root/.ssh/authorized_keys`). Fixed live by running
  `chown root:root /root /root/.ssh /root/.ssh/authorized_keys; chmod 700 /root/.ssh; chmod 600 /root/.ssh/authorized_keys`
  at the top of onstart (repeated for ~2 min in the background). The
  product doesn't need SSH; keep the fix only in dev/spike scripts.
- **Model install:** `POST /api/v2/models/install?source=<url-encoded URL>`
  (JSON body `{}`) → job `{id, status:"waiting"}`. Poll
  `GET /api/v2/models/install` for `status` (`completed`/`error` +
  `error_reason`, `bytes`/`total_bytes`). **Live:** the 4.27 GB SD 1.5
  single-file checkpoint downloaded and registered in **28 s** (~150 MB/s)
  and was auto-selected in the UI. A 404 source fails fast with
  `error_reason: HTTPError`. Letting Invoke download directly (with
  `access_token` for CivitAI) looks simpler than downloading in
  provision.sh. Phase 1 can pick either; the install API reports
  progress, which `/__status` needs anyway.
- `multiuser` mode exists (6.12+) and is off by default. Keep it off; the
  sidecar is the auth boundary.

## 4. Invoke API through a proxy ✅ Live

- List newest-first:
  `GET /api/v1/images/?order_dir=DESC&starred_first=false&is_intermediate=false&limit=N`
  → `{items, total}`, each with `image_name`, `width`, `height`
  (`starred_first` defaults to true, so always pass false).
- Full-res: `GET /api/v1/images/i/{image_name}/full` → `200 image/png`.
  **Live:** a 512×512 image was 392 KB.
- Socket.io at `/ws/socket.io/`. ✅ **Live: websocket upgrade works through
  aiohttp proxy + quick tunnel** (proxy logged `WS OPEN /ws/socket.io/`;
  live progress previews streamed in the UI during generation).
- Spike proxy (aiohttp, ~50 lines: one-time token → HttpOnly Secure cookie
  → HTTP streaming + WS pump) was enough for the full UI: load, generate,
  gallery. The token/cookie gate returned **401** for: no cookie via the
  tunnel, a bad token, and the raw public port.
- ⚠️ **aiohttp is not in the Invoke image.** Available in `/opt/venv`:
  starlette, uvicorn, httpx, websockets, fastapi. **Live:**
  `uv venv /opt/sidecar-venv && uv pip install aiohttp` took **0.8 s**.
  → Sidecar gets its own uv venv with pinned deps, decoupled from Invoke's
  venv.

## 5. cloudflared quick tunnel ✅ Live

- Binary: `https://github.com/cloudflare/cloudflared/releases/download/2026.9.3/cloudflared-linux-amd64`
  (pin version + SHA-256 in Phase 1).
- Run:
  `cloudflared tunnel --no-autoupdate --metrics 127.0.0.1:20241 --url http://127.0.0.1:8080`.
- ✅ **Live URL discovery:** `GET http://127.0.0.1:20241/quicktunnel` →
  `{"hostname":"thunder-west-textile-brothers.trycloudflare.com"}` within
  ~5 s. The log confirmed the metrics server bound to `127.0.0.1`.
  (Without `--metrics`, containers bind `0.0.0.0`, exposing `/debug/pprof`.)
- ⚠️ **DNS lag:** the first request to the new hostname, a few seconds after
  it appeared, got no response (curl `000`). Retrying ~10 s later worked.
  The launcher must retry/poll the tunnel URL before opening the webview.
- Websockets ✅ live. Limits per docs: 200 in-flight requests, no SSE (Invoke
  doesn't use SSE).
- Quick tunnels fail if `~/.cloudflared/config.yaml` exists.
- ToS: "testing and development only", no SLA. **User accepted this risk
  (2026-09-24).** Fallback idea if it ever breaks: direct `-p` port + a
  self-signed cert pinned via WebView2
  `--ignore-certificate-errors-spki-list` (🧪).
- Publishing the URL: the instance can `PUT` its own `label` with the
  hostname (✅ label write works with the instance key), and the launcher
  reads it via `GET /instances/{id}/`. No extra port needed.

## 6. Tauri 2 remote webview ✅ docs / 🧪 local

- `WebviewWindowBuilder::new(app, "remote", WebviewUrl::External(url))`.
- Remote origins get no command access unless a capability lists them
  under `remote.urls`. Give the `remote` window label **no capability**.
  `__TAURI_INTERNALS__` is still injected, but ACL denies calls. (Advisory
  GHSA-57fm-592m-34r7, iframe bypass, is fixed.)
- Cookies: dedicated `data_directory` for the remote window, or
  `incognito(true)`. `on_navigation` pins the window to the tunnel origin.
- 🧪 Needs a local check ($0, no rental), but Rust isn't installed on this
  PC. **First task of Phase 2.**

---

## Phase 1 acceptance (2026-09-24, instance 52519537) ✅

Bundle `instance-v0.1.0` (SHA-256 `2ff1ecf6…3811`); model Banana Splitz XXL
1.2.1 via CivitAI; RTX A4000 in Delaware, image cached on the host.

| Step | Result |
| --- | --- |
| Create → tunnel label published | **+50 s** (onstart → verify bundle → provision.sh → sidecar → cloudflared → `PUT label`) |
| 6.94 GB CivitAI download + SHA-256 | done by +131 s; token via header file, redirect followed |
| Register via install API (`inplace=true`) | +152 s → +172 s |
| **Ready** | **+172 s** |
| No cookie (`/`, `/api/v1/images/`), `/__ticket`/`/__status` without bearer | 401 |
| Fresh ticket → cookie → Invoke UI | ✅ model auto-selected (SDXL, 1024²) |
| Generate | ✅ 1024×1024 |
| Inpaint (canvas mask + prompt) | ✅ graph used `create_gradient_mask`/`apply_mask_to_image`; queue 2 completed / 0 failed |
| Launcher-style bearer: list + `/full` | ✅ 200, 1.8 MB PNG |
| Ticket 65 s old | 401 |
| **Heartbeat killed → self-destroy** | **gone 182 s after the kill** (heartbeat timeout 3 min + ≤15 s tick) |

Notes for later phases:
- ⚠️ **Bandwidth is billed per GB and varies ~15× between hosts.** Offer
  `inet_down_cost` ranged $0–$0.039/GB in one search. On an expensive host,
  a 6.9 GB model costs ~$0.27 per session to download, more than an hour of
  a cheap GPU. This run's credit drop ($11.757 → $11.553, $0.20) was mostly
  bandwidth, plus late-posted charges from the dead-host instance.
  → Phase 2 offer ranking must use
  `dph_total × expected_hours + model_GB × inet_down_cost`, and the cost bar
  must include the one-off download cost.
- Canvas results stay **staged** (not in the gallery or the images list)
  until the user accepts them. → Phase 4 sync needs to decide whether to
  also pull staged/intermediate canvas outputs, or rely on acceptance.
- Idle timeout was verified by unit tests only (20 min live would have
  cost more for little new information).
- Invoke's `/api/v1/queue/default/status` returns
  `{queue: {pending, in_progress, completed, failed, …}}` as expected.

## Rental log (all destroyed; none running)

| Instance | Offer | Outcome | Cost |
| --- | --- | --- | --- |
| 52507388 | 44306809 (RTX 3060, UK) | Bad tag `v6.14.1` → `manifest unknown`; destroyed from PC | ~$0.002 |
| 52509466 | 48328449 (RTX A4000, US) | Ran; SSH refused (authorized_keys perms); destroyed from PC | ~$0.006 |
| 52510101 | 48328449 | Full test sequence passed; **self-destroyed via instance key** | ~$0.013 |

Total: **~$0.02** (credit $11.787 → $11.766). No API keys were created on
the account.

Phase 1:

| Instance | Offer | Outcome | Cost |
| --- | --- | --- | --- |
| 52513014 | 37862807 (RTX A4000, JP) | Dead host: stuck `loading`, container never created; destroyed from PC | ~$0.01–0.04 |
| 52519537 | 48328454 (RTX A4000, US) | Full acceptance passed; **self-destroyed on heartbeat loss** | ~$0.17–0.20 incl. bandwidth |

Phase 1 total ~$0.21 (credit $11.766 → $11.553).

## Decisions (user, 2026-09-24)

- **§9.1 watchdog credential → `CONTAINER_API_KEY`** (condition met: proven
  instance-scoped and able to self-destroy). The sidecar reads it from its
  own environment (inherited from PID 1 via onstart). Launcher-minted keys
  are not needed.
- **§9.2 transport → cloudflared quick tunnel**, accepting the ToS risk.
  Tunnel URL published via the instance label.

- **§9.3 content policy → link the rules, no in-app policy text.** Wizard
  links: Vast ToS `https://vast.ai/terms`, CivitAI ToS
  `https://civitai.com/content/tos` (both checked live, 200).
- **§9.4 starting model → Banana Splitz XXL 1.2.1** (CivitAI model 1261882,
  version 3114420, Illustrious/SDXL, 6.94 GB fp16, SHA-256 in
  `catalog/catalog.json`). Downloading without a token returns **401**, so the
  user's CivitAI key is required. It is flagged NSFW on CivitAI. License
  allows use on rented GPUs. The user's link was on civitai.red, which
  serves the same model data as civitai.com; the catalog uses civitai.com
  URLs.
- **§9.5 app name → SlopTweak.** GitHub org/repo not chosen yet; needed
  before the catalog URL is hard-coded (Phase 3), not before.
- **§9.6 signing → deferred to Phase 5.** It doesn't block anything
  earlier.

## Still open

- GitHub org/repo for releases and `catalog.json` (needed by Phase 3).
