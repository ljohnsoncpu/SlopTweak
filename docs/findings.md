# Phase 0 Findings

Status: **complete (2026-09-24).** Docs pass plus a live Vast rental.
Items 1–5 are verified live. Item 6 (Tauri) was verified locally at the
start of Phase 2.

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
the next offer, and cap `loading` at ~8 min regardless. (Superseded in
Phase 2 by the stall-aware check; see "Startup policy".)

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

## 6. Tauri 2 remote webview ✅ Local (Phase 2)

- `WebviewWindowBuilder::new(app, "remote-N", WebviewUrl::External(url))`
  with `.data_directory(<app_local_data>/remote-webview)`,
  `.on_navigation(|u| u.origin() == tunnel_origin)`, and
  `.on_new_window(|_, _| NewWindowResponse::Deny)` (all in tauri 2.11.6).
- App commands are declared in `build.rs` via `AppManifest::commands`, so
  each one needs an `allow-*` permission. Only `capabilities/main.json`
  (windows: `["main"]`) grants them. Remote windows match no capability.
- ✅ **Verified locally** by `app/dev/webview-check.mjs` (21/21). The check
  runs the real `sidecar.py` on 127.0.0.1 and inspects the remote window
  over a dev-only CDP port:
  - `__TAURI_INTERNALS__` is injected, but every call is denied, both app
    commands (`get_snapshot not allowed on window "remote-0" … allowed on:
    [windows: "main"]`) and core ones (`window.close not allowed`).
  - Ticket login → HttpOnly cookie; the ticket is single-use; the cookie
    survives reload and isn't visible to page JS. Chromium accepts the
    `Secure` cookie on `http://127.0.0.1`.
  - `location.href = 'https://example.com/'` is blocked; `window.open` makes
    no new target.
  - Separate WebView2 user-data dirs: `%LOCALAPPDATA%\com.sloptweak.launcher\EBWebView`
    (main) vs `…\remote-webview\EBWebView`.
- ⚠️ `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` applies to every WebView2
  environment in the process, so two windows can't each get a debug port
  that way. Use per-window `additional_browser_args` instead. That's why
  `main` is created in `setup()` (`"create": false` in the config), with
  debug-only `SLOPTWEAK_MAIN_DEBUG_PORT` / `SLOPTWEAK_REMOTE_DEBUG_PORT`.
- PowerShell P/Invoke gotcha (dev scripts only): `$null` passed to a `string`
  parameter becomes `""`. Use `[NullString]::Value` for `FindWindow`.

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

## Phase 2 acceptance (2026-09-25) ✅

Driven by `app/dev/vast-acceptance.mjs` (debug build, real Vast, CDP on
both windows; heartbeat timeout 3 min and max session 60 min for the run).
**15/15 checks passed.**

| Run | Result |
| --- | --- |
| R1 Start → Invoke → Stop | Instance 52531231 (RTX 3060, Connecticut, 2.4 Gbps). Cold host: create → `running` 355 s (image pull + SSH layer), then 159 s to Ready (download 6.94 GB + verify + register): **514 s total**. Invoke (v6.14.1, model registered) visible in the app window; IPC denied from the remote page; **Stop → gone in 5 s**, record cleared. |
| R2 crash → relaunch | Instance 52532104, **Ready in 130 s** (same machine, image cached: `running` after 15 s). App killed; relaunch showed "A GPU from an earlier session is still running… Reconnect / Shut it down". **Reconnect** reattached (the stored launch secret worked, Invoke visible again). Killed again; relaunch → **Shut it down** → gone, banner cleared. |
| R3 crash, no relaunch | Instance 52532417. App killed at Ready; **watchdog destroyed it 194 s after the kill** (3 min heartbeat + ≤15 s tick). Relaunch: no orphan, stale record cleared silently. |

Findings from the run:
- ⚠️ **Slow hosts miss the 8-minute `loading` cap because of the image
  pull, not because they're dead.** First attempt (min 500 Mbps): three
  hosts (two on the same KR machine, 560 Mbps; one VN, 868 Mbps) all hit
  the cap. `status_msg` showed Docker pull progress
  (`…: Download complete`), not an error. The app destroyed each one and
  failed cleanly after 3 tries (~$0.007). Fixes (the first fix, a 2 Gbps
  floor, was reverted per the decision below):
  - **Stall-aware loading check** (see "Startup policy" below).
  - A failed attempt now excludes the whole `machine_id`, not only the
    offer id (one machine lists several offers).
  - Provisioning logs `actual_status`/`status_msg` changes, so the pull,
    the SSH layer build, and `running` are visible in the log.
- The provisioning `status_msg` sequence on a cold host is: layer pulls →
  apt output from Vast's SSH wrapper build (`#7 DONE 31.8s`) →
  `Successfully loaded <image>` → `success, running …/ssh`.
- The offer's `dph_total` excludes storage. The app adds
  `storage_cost × disk / 730`, and the result matched the instance's
  `dph_total` to within ~$0.002.
- Offers carry `machine_id` and `host_id`. There's still no field for
  "image cached on this host" (🧪 unverified whether one exists).

## Phase 2 rental log (all destroyed; none running)

| Instance | Offer | Outcome |
| --- | --- | --- |
| 52527438 | 41437596 (RTX 3060, KR, 560 Mbps) | Loading cap (slow image pull); destroyed by the app |
| 52528249 | 41678963 (same KR machine) | Loading cap; destroyed by the app |
| 52529084 | 52063421 (RTX 2060, VN, 868 Mbps) | Loading cap; destroyed by the app → session Failed after 3 tries |
| 52531231 | 47594081 (RTX 3060, US-CT, 2.4 Gbps) | R1 passed; destroyed by Stop |
| 52532104 | 47594081 | R2 passed; destroyed via orphan "Shut it down" |
| 52532417 | 47594090 (same machine) | R3 passed; self-destroyed by the watchdog |
| 52590528 | 51832512 (GTX TITAN X, 500 Mbps floor) | Startup-policy re-test: 634 s loading (past the old 8-min cap, progressing), Ready at **896 s**; R1 checks passed; destroyed by Stop |

Phase 2 total **~$0.124** (credit $11.5389 → $11.4149; late charges may
post).

## Startup policy (user decision, 2026-09-25)

The user prefers cheap to fast: **up to 15 minutes to start is fine**, and
the app shouldn't over-filter on link speed.
- `min_inet_down_mbps` is back to **500**.
- The fixed 8-minute `loading` cap is replaced by:
  - **stall**: fail if `actual_status`/`status_msg` hasn't changed for
    **5 min**. Dead hosts are silent, while pulling hosts update every few
    seconds. This still catches the Phase 1 dead-host case.
  - **hard cap**: 12 min of `loading`. This leaves about 3 min of the
    15-minute per-attempt budget for download, verify, and register (159 s
    on a 2.4 Gbps host, 262 s on the slow host).
  - Unchanged: `Error response from daemon`, and loading with
    `intended_status=stopped`, fail immediately.
- Re-tested live on the cheapest pick, a GTX TITAN X at $0.0626/hr: Ready
  at 14.9 min, which is inside the budget but only just. On a similar host,
  a retry would push the total past 15 min.
- **GPU architecture floor** `min_compute_cap = 750` (Turing / RTX 20-series
  and newer, Vast units). Without it, the cheapest pick was Maxwell
  (compute 5.2). The pinned image uses **torch 2.7.1+cu128** (Invoke
  v6.14.1 `pyproject.toml`). That probably still runs sm_5x, but SDXL is
  slow there, and torch 2.8+ drops Maxwell/Pascal. With the floor, 54
  offers still qualify; the cheapest was an RTX 2060 at $0.062/hr.
  (🧪 Unverified: whether generation works on sm_52 with this image.
  Invoke logs to a file, not the container log, so `request_logs` didn't
  show the device.)
- Faster startup, if we want it later: hosts with the image cached reach
  `running` in ~15 s instead of 6–10 min. There's no known offer field for
  that; a per-machine "was fast before" memory is one option.
- Dev harness note: `webview-check.mjs` flaked twice, right after a
  rebuild or the mock-UI run (cold WebView2 start), then passed 6 runs in a
  row.

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
- **§9.5 app name → SlopTweak.** Repo `ljohnsoncpu/SlopTweak`; catalog URL
  decided 2026-09-25 (see Phase 3).
- **§9.6 signing → deferred to Phase 5.** It doesn't block anything
  earlier.

## Phase 3 findings (2026-09-25)

- **Catalog URL (user decision):** raw `main` of this repo,
  `https://raw.githubusercontent.com/ljohnsoncpu/SlopTweak/main/catalog/catalog.json`
  (200 live). Fetched on launch (10 s timeout, 1 MB cap), validated, cached
  in `%LOCALAPPDATA%\com.sloptweak.launcher\catalog-cache.json`; falls back
  to the cache, then to the bundled copy. GitHub's CDN caches raw files for
  ~5 min, so an upstream edit can take that long to show up.
- ⚠️ **Divergence from PLAN §4 schema:** `files[].sha256` and `size_bytes`
  are **required**, not optional. provision.sh verifies both, and CivitAI
  supplies them. The app also skips (and reports) entries that point the
  CivitAI key at a non-civitai.com URL, have unsafe file names, lack a
  `main` file, or need a newer Invoke (`invoke_min_version` > 6.14.1).
- **CivitAI key check ✅ Live:** `GET https://civitai.com/api/v1/me` with
  `Authorization: Bearer` → 200 `{id, username, email, tokenScope, …}`; bad
  or missing key → 401 `{"error":"Unauthorized"}`. The app keeps only
  `username` (the reply includes the email).
- **LoRA metadata ✅ Live:** `GET /api/v1/model-versions/{id}` is public and
  has `model.type` (`LORA`), `baseModel` ("SDXL 1.0", "Illustrious", …),
  `trainedWords`, and `files[]` with `hashes.SHA256`, `sizeKB`,
  `downloadUrl`, `primary`, `metadata.format`. **`sizeKB × 1024` is the
  exact byte count** (Detail Tweaker XL: 223097.9921875 → 228,452,344,
  matching a ranged GET's `Content-Range`). So LoRAs go through the
  existing download + size + SHA-256 path in provision.sh unchanged, and
  the instance asset bundle didn't need a new release.
  `GET /api/v1/models/{id}` → `modelVersions[0]` is the newest version.
  Downloads 307-redirect to a presigned R2 URL (HEAD on it → 403, so use a
  ranged GET to probe sizes). Only `.safetensors` LoRAs are accepted.
- A LoRA that fails to download, verify, or register ends the session
  instead of retrying on another GPU (it would fail the same way and cost
  money); the message tells the user to turn it off in Settings.
- **Vast deep links:** `https://cloud.vast.ai/` (sign up),
  `https://cloud.vast.ai/billing/` (Add Credit), and
  `https://cloud.vast.ai/manage-keys/` (API keys, from the Vast quickstart
  docs). Logged out, all of them redirect to `/create/`. Vast's minimum
  deposit is **$5**, and email verification is required before renting.
  CivitAI: `https://civitai.com/login` and `https://civitai.com/user/account`
  (API Keys section).
- Low-balance gate: default floor **$1.00** (Settings). Start is refused
  below it (checked in Rust, not only the UI). The home screen warns when
  credit − download < 1 h at the cheapest offer's price. The cost bar
  counts from instance **creation** (billing starts then, not at Ready),
  refreshes credit every 3 min, and also shows in the Invoke window's
  title, since that window can't show app UI.
- **Mock mode is isolated:** Credential Manager service `SlopTweak-mock`
  and `…\mock` folders, so mock checks never touch real keys or settings.
  (Before this, a mock-mode key check would have overwritten the real
  CivitAI key.)
- Dev harness: launching the app before vite is serving leaves the main
  window blank (main.ts never runs); the scripts wait for vite first.

- **Reliability floor → 99% (user decision, 2026-09-25)**, up from 98%.
  Live that day: 53 offers passed at 0.99 vs 58 at 0.98. The cheapest
  stayed at ~$0.061/hr (RTX 3060, PL, 0.999), so it costs almost nothing.

## Phase 3 acceptance (2026-09-25) ✅

`app/dev/fresh-profile-acceptance.mjs`, debug build, real Vast.
**Approximation of "fresh Windows user profile":** the app's Credential
Manager entries (`*.SlopTweak`) and its whole `%APPDATA%` and
`%LOCALAPPDATA%` folders (settings, catalog cache, both WebView2 profiles)
were deleted. The user's Windows profile itself wasn't new, and "install"
was the debug exe, not the NSIS installer.

| Step | Result |
| --- | --- |
| Wiped profile → launch | wizard shown, no keys stored ✅ |
| User pasted both keys in the wizard (checked on paste) | both stored ✅; catalog fetched from GitHub (`online`) ✅ |
| Add LoRA from a CivitAI link (Detail Tweaker XL) | real metadata, 228,452,344 B ✅ |
| Start (run 1, old build) | 3 attempts all dropped by the 5-min stall rule (see below); run stopped, all destroyed |
| Start (run 2, `--keep-profile`, 99% floor + pull-aware stall) | TH RTX 3060 and NV RTX 2080 Ti hit the 12-min loading cap; KR RTX 2060 (image cached from run 1) `running` in 26 s, **Ready 642 s after create (10.7 min)** ✅ |
| Invoke registered model **and LoRA** | `main:bananaSplitzXXL_121`, `lora:add-detail-xl` ✅ |
| First image (user prompt "A potted plant on a desk") | ✅ 1024² |
| Cost bar | `$0.063/hr · 24 min · ≈$0.05 so far · $11.35 left` ✅ |
| Stop | instance destroyed ✅ |
| App restart | keys still there, no wizard ✅; the stale record from the killed run 1 was cleared on launch ✅ |

Run 2: 9/9 checks. Both runs: credit $11.4149 → $11.3444 (~$0.07).

Findings from the run:
- ⚠️ **Cold image pulls are the startup bottleneck, and `status_msg`
  goes silent while they unpack.** On four cold hosts (KR 2060 ×2, KR
  3060, TH 3060, NV 2080 Ti) `status_msg` froze for 5–10+ min after the
  last `…: Download complete`/`Pull complete`. The 5-min stall rule killed
  working hosts, so it now allows 10 min once Docker pull output has been
  seen (dead hosts show an empty `status_msg` and keep 5 min). The 12-min
  cap still applies.
- **A host with the image cached starts in ~30 s.** Failed attempts leave
  the image cached on the host.
- **Host floor raised (user decision, 2026-09-25):** `inet_down ≥ 2000`
  Mbps (was 500) and a new `disk_bw ≥ 2000` MB/s (Vast `disk_bw` is in the
  offer). Price barely predicts speed. Live median `disk_bw` was 1,754 MB/s
  under $0.10/hr vs ~2,200–3,000 above, and median `inet_down` was ~900
  Mbps in every price band. So filter on specs, not price. 25 of 150
  offers passed both; the cheapest was ~$0.083/hr (+$0.02 over the
  cheapest overall). Live estimate after the change: RTX 3060,
  $0.107/hr incl. storage.
- **Machine memory (user decision):** `machines.json` in app data. Skip a
  machine for 7 days after it fails (unless it has worked since). Prefer a
  machine that reached Ready before when its expected session cost is
  within $0.05 of the cheapest. Stops, bad LoRAs, and key errors aren't
  counted against the machine.
- ⚠️ **Phase 2 bug fixed:** provision.sh reports `progress` as a fraction
  (0–1); the app treated it as percent, so the download bar showed 0% on
  real Vast. The sidecar client now converts it.
- The 7 GB CivitAI download on the 589 Mbps KR host took ~6 min (vs ~1 min
  on 2+ Gbps hosts), which is another reason for the link floor.
- Dev harness: killing the app mid-run left the old script retrying
  instead of cleaning up; it now aborts when the app exits. Instance
  52605435 was destroyed by hand via the API.

## Phase 3 rental log (all destroyed; none running)

| Instance | Offer | Outcome |
| --- | --- | --- |
| 52603419 | 35050679 (RTX 2060, KR) | Run 1: stall rule after `Pull complete`; destroyed by the app |
| 52604442 | 49698940 (RTX 2060, KR) | Run 1: same; destroyed by the app |
| 52605435 | 41437590 (RTX 3060, KR) | Run 1: stopped for the rebuild; destroyed via API |
| 52606728 | 51708220 (RTX 3060, TH) | Run 2: 12-min loading cap; destroyed by the app |
| 52607839 | 49623260 (RTX 2080 Ti, NV) | Run 2: 12-min loading cap; destroyed by the app |
| 52609457 | 35050679 (RTX 2060, KR, image cached) | Run 2: **acceptance passed**; destroyed by Stop |

Phase 3 total **~$0.07** (credit $11.4149 → $11.3444; late charges may post).

## Phase 3 follow-up: live filter check (2026-09-25) ✅

`vast-acceptance.mjs r1`, with the 99% / 2 Gbps / 2000 MB/s filters and
machine memory. It now backs up and restores the user's `settings.json`.

| Check | Result |
| --- | --- |
| Pick | offer 44022327, RTX 3060, New Jersey, $0.1066/hr + $0.018 download; instance 52614236 |
| Host | disk_bw 3755 MB/s ✅, reliability 0.997 ✅, inet_down **1969** Mbps on the instance (the offer passed the ≥2000 filter; Vast re-measures, so the check allows 5% drift) |
| Cold start → Ready | **336 s (5.6 min)**, vs 10.7 min or failures on sub-2 Gbps hosts earlier |
| Invoke visible, remote IPC denied, model registered | ✅ |
| Invoke window title | ✅ `SlopTweak — Invoke · $0.107/hr · 6 min · ≈$0.03 so far · $11.33 left` (read via EnumWindows; PowerShell must output UTF-8 or "—"/"·" get mangled) |
| Stop → destroyed, record cleared, `machines.json` marks the machine good | ✅ |

Spend ~$0.018 (credit $11.3423 → $11.3247). The title check is also in
`mock-ui-check.mjs` now (45/45).

## Phase 3 rental log, follow-up

| Instance | Offer | Outcome |
| --- | --- | --- |
| 52614236 | 44022327 (RTX 3060, NJ, 3755 MB/s) | Filter check passed; destroyed by Stop |

## Still open

- **Live upstream catalog edit.** Fetching from GitHub works live, and an
  upstream edit shows up without a rebuild against a local server
  (mock-ui-check). Pushing a test edit to `main` was blocked by the agent's
  permission policy (it changes what every install sees), so the user does
  that step: edit `catalog/catalog.json` on `main`, wait ≤5 min for GitHub's
  CDN, then press Settings → "Check for new models".
- Wizard screenshots of the logged-in Vast/CivitAI pages. Claude in Chrome
  showed no connected browser. Drop PNGs into `app/src/wizard/`
  (`vast-signup`, `vast-billing`, `vast-keys`, `civitai-signup`,
  `civitai-keys`); the wizard shows them automatically.
