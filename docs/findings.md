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
  ⚠️ **Corrected in Phase 4:** accepting doesn't put them in the gallery
  either. See "Phase 4 findings".
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
  earlier. **Decided 2026-09-25:** free signing only, via the **SignPath
  Foundation** open-source program (the certificate is issued to SignPath
  Foundation, which Windows shows as the publisher; the user accepted
  that). No paid options (Azure Artifact Signing is $9.99/month; the user
  has no Azure credit). The Microsoft Store (free for individuals since Sept
  2025, Store-signed MSIX) is the fallback, at the cost of the GitHub
  updater and a Store content review. SignPath needs an OSI licence (done:
  MIT, below), a published release, a download page describing the app,
  and 2FA on GitHub; the user applies at signpath.org/apply. Risk: an OV
  certificate still builds SmartScreen reputation over downloads, so early
  installs may warn.
- **Licence → MIT (user decision, 2026-09-25).** Copyright holder "The
  SlopTweak contributors" (the user preferred it to their GitHub handle).
  `LICENSE` at the repo root; `license = "MIT"` in `app/package.json`,
  `app/src-tauri/Cargo.toml`, and `instance/pyproject.toml`.

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

## Catalog: Anima (user decision, 2026-09-25) ✅ Live

- **§9.4 model set extended:** Anima Aesthetic v1.1 and Anima Turbo v1.1
  (`circlestone-labs/Anima`, 2B, Cosmos-Predict2 based, anime-focused), both
  added at the user's request. **Chroma** (lodestones) was skipped: Invoke
  6.14.1 has no loader for it. The **Krea** models (FLUX.1 Krea dev, Krea-2)
  were skipped for now: gated on Hugging Face (the instance would need an HF
  token), much bigger, and need 16–24 GB+ GPUs.
- License: **CircleStone Labs Non-Commercial License v1.2**. Personal and
  hobby use is fine; commercial or production use isn't. Renting your own GPU
  isn't "Distribution" (that means hosting it for third parties).
- **Invoke 6.14.1 support is native:** `BaseModelType.Anima`, and it needs 3
  files, all installable via the install API: the main transformer
  (`anima/main`, 4.18 GB), the Qwen3 0.6B text encoder (`any/qwen3_encoder`,
  1.19 GB, variant `qwen3_06b`), and the Qwen-Image VAE (`anima/vae`,
  0.25 GB). The Anima loader is present from 6.13.8; the catalog says 6.14.1
  (the verified version).
- Files come from Hugging Face, pinned to revision
  `f973fc41ec7545364ac9776c2440285f43ff2a30`, with SHA-256 from the HF API
  (LFS oid). Sizes were confirmed by ranged GET. No token is needed.
- Invoke runs Anima in **bf16 when the GPU allows it**
  (`choose_bfloat16_safe_dtype`); Turing only emulates bf16. So the catalog
  gained an optional per-model **`min_compute_cap`**. Anima sets 800
  (RTX 30-series+), which raises the app-wide 750 floor for that model only
  (it can't lower it). The model selector names the GPU class.
- **Live (instance 52617607, offer 49992718, RTX 3060, Utah, 7.6 Gbps /
  6.6 GB/s, $0.1012/hr):** Ready in 324 s; all 3 files registered; with
  Invoke's defaults (1024², Anima encoder and VAE auto-selected) a prompt
  typed into the UI made an image in **64 s**; cost-bar title, Stop, and
  machine memory ✅. 11/11 checks, ~$0.028 (credit $11.3235 → $11.2956).
  Anima Turbo wasn't run live (same pipeline, different main file); it needs
  CFG 1 and 8–12 steps, which its description tells the user.
- Follow-up: per-model `default_settings`. Built; see "Model default
  settings" below.
- CivitAI's `baseModel` "Anima" maps to the `anima` family for LoRAs.

## Catalog research: Krea-2 and Kroma (2026-09-26, source only, not live)

This supersedes the "Krea-2 is gated" note above.

- **Invoke 6.14.0+ supports Krea-2 natively** (`BaseModelType.Krea2`,
  variants `krea2_turbo` / `krea2_base`). The pinned 6.14.1 has it. It
  accepts a diffusers folder, a single-file `.safetensors` (bf16 or ComfyUI
  "scaled fp8"), or GGUF. Single-file and GGUF need a separate Qwen3-VL-4B
  encoder and the Qwen-Image VAE.
- **Kroma** (`lodestones/Kroma`, rev `b921d45c…`) is **not gated**. Its v0.3
  files are `kroma-v0.3-turbo.safetensors` (25.64 GB, bf16, 430 tensors) and
  `kroma-v0.3-base.safetensors` (51.3 GB). The keys are in the native/ComfyUI
  layout (`blocks.*`, `txtfusion.*`, `first.*`, `tproj.*`). Invoke's
  `_has_krea2_keys` matches `txtfusion` together with `first`/`tproj`, and
  `Krea2CheckpointModel` converts native keys to diffusers keys. The variant
  comes from the filename ("turbo" → Turbo, "base"/"raw" token → Base), so the
  filenames must be kept.
- **Krea's own repos** (`krea/Krea-2-Turbo`, `-Raw`) are `gated: auto`, so
  they need an HF token. **`Comfy-Org/Krea-2` is not gated** and has single
  files: `text_encoders/qwen3vl_4b_bf16.safetensors` (8.88 GB),
  `qwen3vl_4b_fp8_scaled.safetensors` (5.24 GB), `vae/qwen_image_vae.safetensors`
  (253.8 MB), plus Krea-2 Turbo/Raw bf16, fp8_scaled (13.1 GB), and nvfp4
  transformers.
- **VAE:** `krea2/vae_compat.py` accepts the Anima-classified Qwen-Image VAE.
  The Anima catalog entry's `qwen_image_vae.safetensors` already works.
- **Encoder caveat:** the single-file Qwen3-VL loader fetches config and
  tokenizer from `Qwen/Qwen3-VL-4B-Instruct` (ungated, Apache-2.0) **at load
  time**, which is unpinned. The alternative is to install that repo as a
  folder (8.88 GB, pinnable to rev `ebb281ec…`).
- **VRAM:** the bf16 Kroma/Krea-2 transformer (25.6 GB) doesn't fit on 24 GB
  without Invoke's per-model FP8-storage setting (≈12.8 GB), which also needs
  GPU support (`_device_supports_fp8_storage`). Kroma ships no fp8 file.
- **License:** HF lists Kroma as `krea-2-community-license` (base weights);
  the fine-tune delta is MIT. The Krea-2 license terms haven't been reviewed.

### Krea 2 and Kroma live (2026-09-26) ✅ both work

`TEST_MODEL=<id> GENERATE=1 LOCAL_CATALOG=1 node dev/vast-acceptance.mjs r1`,
11/11 each. Total spend ~$0.58 across all runs below (credit $10.3671 → $9.7833).

- **Registration:** all 3 files per model install through the API as
  `krea-2/main` (variant `krea2_turbo` for both; Kroma's is read from the
  filename), `any/qwen3_vl_encoder`, `anima/vae`. The Qwen3-VL config fetch at
  load time worked.
- **Krea 2 Turbo** (fp8_scaled) on an RTX 3090 (24 GB, $0.199/hr): Ready in
  447 s; first image 187 s including the model load.
- **Kroma Turbo** (bf16, 25.6 GB) on an RTX 3090 ($0.315/hr, 8.8 Gbps): Ready
  in 541 s; first image 164 s including the load. It runs on 24 GB, but
  only because Invoke offloads part of it, and the user reports slow gens.
  **Catalog now asks for 32 GB** (tier $$$; 5090 ≈ $0.59–0.65/hr, A6000
  $0.48–0.57/hr on 2026-09-26).
- **Kroma on 48 GB** (RTX A6000, $0.569/hr, 2.4 Gbps): Ready in 905 s (the
  download alone ~12 min, so a flat 15 min is too tight). First image 99 s
  including the load (vs 164 s on the 3090); **warm image 12 s at 8 steps /
  CFG 1**, clean. ~$0.22.
- ⚠️ **Ready timeout:** Kroma's first try (RTX 3090, 2.3 Gbps listed) took
  ~14 min to download 31 GB (~36 MB/s) and was dropped at the flat 15 min.
  The timeout is now `max(setting, 8 + 0.6 min/GB)` (`config::ready_timeout_minutes`;
  Kroma ≈ 27 min, Krea 2 ≈ 20). The 5-min reattach deadline is unchanged.
- ⚠️ **Invoke never applies a model's default settings by itself.** Both
  models were registered with `default_settings` `steps 8, cfg_scale 1`, but
  the first image used 30 / 7.5 (Krea 2's came out oversaturated with hatching
  artefacts). In 6.14.1 only `UseDefaultSettingsButton` (the sparkle icon next
  to the model picker) dispatches `setDefaultSettings`. `modelsLoaded` →
  `modelSelected` doesn't. Its `aria-label` key `modelManager.useDefaultSettings`
  is missing from `en.json`.
- **Fix (as built): `defaults.js`**, injected after `tutorial.js`. Once per
  origin (a new GPU is a new tunnel origin), when Invoke's UI is up, it reads
  the main model's `default_settings` and posts `steps`/`cfg_scale` to
  `POST /api/v1/recall/default`. Invoke stores them as `recall_*` client state
  and emits `recall_parameters_updated` to connected pages
  (`setEventListeners.tsx` dispatches `setSteps`/`setCfgScale`). The event is
  live-only, so the script sends it twice (2 s and 7 s after the UI is up).
  ✅ **Live:** the POST returned 200 and the open page's Steps/CFG fields
  changed to 8/1; the next image used 8/1. ⚠️ Invoke's **Invoke button has
  no aria-label** in 6.14.1, only the text "Invoke" (the first live try
  waited on the aria-label and never fired). ✅ **End to end** (Krea 2,
  RTX 3090, 12/12): the script set 8/1 by itself ~8 s after the UI was up,
  the image used 8/1 and came out clean (55 s including the load). ~$0.07.

| Instance | Offer | Outcome |
| --- | --- | --- |
| 52841256 | 30086286, RTX 3090, $0.199/hr | Krea 2 run; destroyed by Stop |
| 52842700 | 30086286, RTX 3090, $0.201/hr | Kroma; not ready in 15 min, destroyed by the app |
| 52844373 | 52493301, RTX PRO 4000, $0.306/hr | Kroma retry; run stopped, destroyed via API |
| 52844585 | 52741273, RTX 3090, $0.315/hr | Kroma run; destroyed by Stop |
| 52847960 | 52313037, RTX A6000, $0.569/hr | Kroma 48 GB speed run; destroyed by Stop |
| 52850783 | 52741273, RTX 3090, $0.312/hr | Krea 2 automatic-defaults run; destroyed by Stop |

## Phase 4 findings (2026-09-25)

### Canvas results and the gallery (Invoke 6.14.1, source + live)

- ⚠️ **Canvas "Accept" never saves to the gallery.** `onAccept` in
  `StagingArea/context.tsx` only adds the result as a raster layer and resets
  the staging session. A canvas result reaches the gallery only via the
  staging toolbar's **Save To Gallery** (floppy; it re-uploads a copy as
  `general`, non-intermediate), the canvas right-click **Save To Gallery →
  Save Canvas To Gallery / Save Bbox To Gallery**, or the **Send To Gallery**
  mode (`saveAllImagesToGallery`: outputs are non-intermediate and skip
  staging).
- Staged canvas outputs are `general` + `is_intermediate=true`
  (`selectCanvasOutputFields`), made by a node whose *source* id is
  `canvas_output:<id>`.
- ⚠️ **An image record's `node_id` is the prepared node id (a uuid), not
  the source id**, so `canvas_output` can't be found in the image list.
  (The first live run synced 0 Canvas files because of this.) The
  session queue has the mapping: `GET /api/v1/queue/default/i/{id}` →
  `session.prepared_source_mapping` (prepared → source) and
  `session.results[prepared].image.image_name`. ✅ **Live** (instance
  52630531): a canvas inpaint item had 16 nodes, one `canvas_output` source
  with a uuid prepared id, and its image was `is_intermediate: true`,
  `general`. `GET /api/v1/queue/default/item_ids?order_dir=DESC` →
  `{item_ids, total_count}` is a cheap way to see new items.
- **User decision:** save gallery images to the output folder and *every*
  Canvas try (accepted or not) to its `Canvas` subfolder. A canvas result
  that's already non-intermediate (Send To Gallery mode) is left to the
  gallery sync.
- Images list API: without `board_id` it lists all boards; `categories=general`
  leaves out uploads (`user`) and masks; `limit` ≤ 1000 (`MAX_PAGE_SIZE`);
  `created_at` is SQLite UTC (`YYYY-MM-DD HH:MM:SS.fff`).
- Invoke has no URL or deep link for "open this image in Canvas"
  (`main.tsx` takes no parameters), so the tutorial gives instructions
  instead of scripting it.

### Output sync (as built)

- Rust, through the sidecar with the launch secret (bearer); never through
  the remote webview. Polls every 10 s while Ready: the gallery (newest
  first, stops at the first page with nothing new), then queue item ids
  (stops at the first item already handled; only finished items are read).
- Bearer requests don't count as activity in the sidecar's idle timer, so
  polling never keeps an idle GPU alive.
- Files: `<output>\YYYY-MM-DD_HH-MM-SS_<invoke name>.png` (local time) and
  `<output>\Canvas\…`. Names are validated (`[A-Za-z0-9._-]`, image
  extension); bytes must look like PNG/JPEG/WebP; writes are temp file +
  rename. The per-instance ledger `%LOCALAPPDATA%\com.sloptweak.launcher\synced\<id>.json`
  survives a restart/reattach and is deleted with the instance.
- **Stop / close / shutting down a leftover GPU:** one last full pass,
  bounded at 60 s (15 s when the GPU is already shutting itself down), then
  destroy regardless. Close waits up to final sync + destroy confirm + 60 s.
  Unit test: a download that hangs for an hour still destroys after 60 s.
- Mock mode saves its 1-pixel fakes to `…\mock\output`, not Pictures.

### Tutorial (as built)

(v1, replaced in Phase 6: see "Tutorial v2 (as built)" below.)

- An overlay injected into the Invoke window as a Tauri initialization
  script (`remote.rs`, `tutorial.js`). It's plain page JS in a shadow DOM:
  the window still has no capability (webview-check 21/21 with it
  injected). It uploads the bundled sample (a room with a vase, drawn by
  `dev/make-tutorial-sample.py`) with Invoke's own upload API as a `user`
  asset, then reloads once. ✅ **Live:** Invoke's gallery doesn't show
  uploads made outside its UI until a reload; after it, the sample is in
  Assets.
- Steps: Assets → right-click → **New Canvas from Image → As Raster Layer
  (Resize)** → select **Inpaint Mask** (new canvases have an empty one) →
  **B** → paint → prompt → **Invoke** (the card moves on by itself when
  `queue.completed` goes up) → **✓ Accept**, with a pointer to Save To
  Gallery. All labels were checked against 6.14.1's `en.json` and live.
- Skip/Done reach the app only as a navigation to
  `/__sloptweak/tutorial/<done|skipped>`, which `on_navigation` intercepts
  and blocks. A page could fake it, but all it does is set
  `tutorial_done`. The overlay auto-shows until then; **Show tutorial**
  reopens it (Rust `eval` into the page if the window is open, else it
  opens with the tutorial forced once). Its per-page state is in the remote
  profile's `localStorage`, which is per tunnel origin, so it's fresh on
  every new GPU.
- ✅ **Live:** Invoke 6.14.1 registers one `beforeunload` listener, but the
  blocked signal navigation raised no "leave site?" prompt; the page stayed.
- Tutorial feedback round (2026-09-26): the welcome card opens centered over
  a dimmed page (the Invoke window covers the app, and a corner card was easy
  to miss). The step cards start bottom-right and can be dragged by their
  title line; the position is kept on screen and saved with the tutorial
  state. A highlight ring marks the next thing to click. It finds the target
  only by visible text or `aria-label` (Assets, New Canvas from Image, As
  Raster Layer (Resize), Inpaint Mask, Invoke, Accept), so a UI change just
  means no ring. It's off while the page is hidden. Checked on a stand-in
  page, **not yet against live Invoke**.
- A last step links to Invoke's docs. Links reach the app as
  `/__sloptweak/docs/<key>`; `remote::invoke_docs` maps each key to a fixed
  URL (the page never sends a URL), and the app opens it in the default
  browser, at most once a second. The docs moved: `invoke-ai.github.io/InvokeAI`
  now 301s to `https://invoke.ai/`, and `support.invoke.ai` no longer
  resolves (2026-09-26). Pages used: `/`, `/features/canvas/layers-and-drops/`,
  `/features/canvas/bounding-box/`, `/concepts/prompting-guide/`,
  `/features/hotkeys/`, `/troubleshooting/videos/` (all 200).

### Templates and workflows (as built, 2026-09-26)

- Invoke keeps prompt templates ("style presets") and workflows in the
  instance's database, so they die with the GPU. Invoke 6.14.1 API (source
  at the tag): `GET /api/v1/style_presets/` lists all (Invoke's own are
  `type: "default"`); `POST /api/v1/style_presets/` is multipart, a `data`
  JSON field (`name`, `positive_prompt`, `negative_prompt`, `type`,
  `is_public`) plus an optional `image` (a 256 px thumbnail; ✅ live, `GET
  …/i/{id}/image` served it back as `image/png`). Non-admins can't create `default` ones; CSV
  `export`/`import` are admin-only, so they aren't used. Workflows:
  `GET /api/v1/workflows/?categories=user&page=&per_page=` (items carry
  `workflow_id`, `name`, `updated_at`), `GET …/i/{id}` → `{workflow: …}`,
  `POST /api/v1/workflows/` with `{"workflow": …}` minus its `id`. Creates
  answer 200. Collection routes need the trailing slash (FastAPI would 307).
- A template's positive prompt holds `{prompt}` where the user's prompt goes;
  without it Invoke appends the user's prompt (`buildPresetModifiedPrompt`).
- `library.rs`, through the sidecar with the bearer secret (the proxy passes
  any method). On Ready, before the Invoke window opens (≤ 30 s), it puts
  back `library.json` (config dir) plus the running model's catalog
  templates, skipping names Invoke already has. Every 60 s and in the final
  pass after the images it saves what changed: new/edited/renamed templates
  (with their image) and workflows (re-read only when `updated_at` moves).
  Only ids seen this session count as deleted, so a failed restore never
  empties the library. A deleted built-in goes to `hidden_builtins`; an
  edited one is saved as the user's under the same name.
- Built-in templates live in the catalog (`models[].templates`: name,
  positive with one `{prompt}`, negative, a ≤ 64 KB base64 JPEG thumbnail of
  the example image, and `image_source`). A bad template is dropped, not the
  model. They reach users only once `catalog.json` is on `main`, since the
  app prefers the online catalog.
- Sources (user decision 2026-09-26): Banana Splitz XXL and Anima Aesthetic
  get one "Showcase tags" template each: the tags common to *every*
  showcase prompt on the CivitAI version (Banana: 10 images → `masterpiece,
  best quality` + 8 shared negatives; Anima Aesthetic: 7 → `masterpiece,
  best quality, safe` + the card's 6 negatives). Thumbnails are safe-rated
  showcase images. Anima Turbo has none: its 9 showcase prompts share no
  positive tag, and at CFG 1 Invoke ignores negatives. Krea 2 Turbo: 7 styles
  from the captioned samples on `krea/Krea-2-Turbo` (no negatives, CFG 1).
  Kroma (no examples on its page) reuses Krea 2's set.
- ✅ **Live (2026-09-26), `dev/library-acceptance.mjs`, 19/19**, Anima
  Aesthetic, two RTX 3070 rentals (Quebec, $0.0973/hr, ready in ~2 min):
  the built-in template and its picture were in Invoke 6.14.1 when the
  window opened; a template and a copied 17-node default workflow made
  through Invoke's API were in `library.json` after Stop (workflow without
  `id`); the second GPU had both back, nothing doubled, and the workflow
  opened with its graph. Tutorial on real Invoke: welcome centered, step
  card bottom-right (16 px), the ring found the Assets tab and the Invoke
  button, the docs link reached the app and the page stayed. Spent ~$0.03.

| Instance | Offer | Outcome |
| --- | --- | --- |
| 52837260 | RTX 3070, Quebec, $0.0973/hr | GPU 1 of the library run; destroyed by Stop |
| 52837502 | RTX 3070, Quebec, $0.0973/hr | GPU 2 of the library run; destroyed by Stop |

### Phase 4 acceptance (2026-09-25) ✅

`app/dev/phase4-acceptance.mjs`, debug build, real Vast, Banana Splitz XXL.
"Clean install" means a clean tutorial state (`tutorial_done=false`, a new
tunnel origin); the keys stayed, as in Phase 3.

| Run | Result |
| --- | --- |
| 1 (instance 52627339) | Ready in 124 s. The tutorial showed by itself and the sample uploaded. The script's submenu click missed, so it stopped; the agent finished the tutorial through CDP (right-click → New Canvas from Image → As Raster Layer (Resize), mask, typed prompt, Invoke → the card moved on in 15 s → Accept → Done → app recorded it). 10 images → Stop → **10/10 on disk** (9 synced while running, the last by the final pass; Stop took 3 s). ❌ The Canvas try wasn't saved (the `node_id` finding above). |
| 2 (instance 52630531) | Fixed sync + script. **Fully automated, 13/13:** tutorial shown, sample uploaded, mask painted, moved on 23 s after Invoke, completed and recorded; the Canvas try saved to `Canvas\`; **10/10 images on disk after Stop** (1.09–1.24 MB PNGs; 9 while running + 1 in the final pass; Stop took 3 s); report `done`, nothing missing; instance destroyed; record cleared. |

$0 checks: `cargo test` 117 passed; clippy -D warnings clean; `sync-check.mjs`
18/18 (real app + real sidecar.py + `fake_invoke.py`); `mock-ui-check.mjs`
50/50; `webview-check.mjs` 21/21.

Dev harness notes:
- A vite left running from another worktree kept :1420 despite
  `--strictPort`, so the dev scripts silently loaded that checkout's UI. The
  scripts now check that :1420 serves this checkout's `index.html`.
- `sync-check` flaked once right after a rebuild (the mock session didn't
  reach Ready in 2 min), then passed. Same cold-WebView2 pattern as
  webview-check in Phase 2.
- ⚠️ The Vast billing page shows **auto top-up enabled** ($10 when credit
  drops below $5). Test spend never got near that; noted because the money
  rule is "existing credit only".

## Phase 4 rental log (all destroyed; none running)

| Instance | Offer | Outcome |
| --- | --- | --- |
| 52627339 | 49992718 (RTX 3060, Utah, $0.1012/hr) | Run 1: 10/10 synced; Canvas sync bug found; destroyed by Stop |
| 52630531 | 49992718 (same machine) | Run 2: 13/13; destroyed by Stop |

Phase 4 total **~$0.084** (credit $11.2925 → $11.2089; late charges may post).

## Phase 5 findings (2026-09-25)

### Decisions (user, 2026-09-25)

- **Updater key → GitHub Actions secrets** `TAURI_SIGNING_PRIVATE_KEY` /
  `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`, plus the user's offline backup. Generated
  locally (never printed), outside the repo; only the public key is committed
  (`tauri.conf.json` → `plugins.updater.pubkey`, key id `36F5919E71F882FB`).
- **Wizard screenshots of Vast/CivitAI → ship without them** (the wizard keeps
  its text steps; the slots in `app/src/wizard/` stay optional). The user guide
  uses screenshots of SlopTweak itself from the mock run.
- **First release → v0.2.0**, unsigned. Asked again before any tag or publish.
- Code signing, per the Phase 5 brief: SignPath Foundation only (see
  Decisions §9.6); the application is the user's to submit.

### Verified

- ✅ **Local updater end to end (0.2.0 → 0.2.1, release builds, $0).** Both
  built like CI (`tauri build --no-bundle` then `tauri bundle --bundles nsis`),
  with only the endpoint overridden (`--config`) to `http://127.0.0.1:8765` +
  `dangerousInsecureTransportProtocol`. 0.2.1's installer was signed with the
  real updater key; `latest.json` came from `.github/scripts/release_files.py`.
  Installed 0.2.0 silently, launched it, and clicked **Update now** via UI
  Automation: the banner said "SlopTweak 0.2.1 is available (you have 0.2.0)",
  the installer downloaded, 0.2.0 exited 0, the passive NSIS installer ran with
  no prompts (a progress window only), and **0.2.1 was installed and relaunched
  within ~9 s** (it re-checked `latest.json` at +9 s and offered nothing).
  Registry `DisplayVersion` 0.2.1. Test install removed afterwards. What's
  **not** verified yet: the real GitHub endpoint (needs two published releases)
  and SmartScreen on a signed build.
- ✅ `tauri bundle` does **not** rebuild the exe (hash unchanged across the
  bundle step), so a SignPath-signed exe can be swapped in between
  `build --no-bundle` and `bundle`. release.yml asserts this on every run.
- The release exe's version resource: `ProductName SlopTweak`,
  `FileDescription SlopTweak`; `CompanyName` was the crate name (`sloptweak`),
  now `bundle.publisher` = "The SlopTweak contributors" (also the uninstall
  entry's Publisher). Installer 3.9 MB, per-user install to
  `%LOCALAPPDATA%\SlopTweak`, no UAC.
- ✅ The updater's `latest/download` URL skips drafts **and pre-releases**, so
  releases meant for auto-update must be published as normal releases.
- The instance asset bundle is reproducible (rebuild = pinned
  `2ff1ecf6…3811`). The app now pins
  `releases/download/v<app version>/instance-assets.tar.gz`; release.yml
  rebuilds it and fails unless the hash equals `ASSETS_SHA256`
  (`build_assets.py --check-pin`, also in CI). Debug builds default to the
  `instance-v0.1.1` pre-release (same bytes), since `vX.Y.Z` doesn't exist
  until it's published. ⚠️ Consequence: a draft release's files aren't public,
  so a release build can only start a GPU after its release is published. The
  runbook does one ~$0.05 real session right after publishing.
- Git Bash rewrites `/S` into a path (`S:/`), so the NSIS silent switch must be
  passed from PowerShell or cmd.
- `navigator.clipboard.writeText` failed in the WebView2 main window when it
  wasn't focused (CDP-driven). Copy diagnostics writes the clipboard from Rust
  (`tauri-plugin-clipboard-manager`) instead; the UI falls back to selecting
  the text.
- The updater and clipboard plugins add JS commands, but no capability grants
  them: webview-check now asserts `plugin:updater|check` and
  `plugin:clipboard-manager|read_text/write_text` are denied from the Invoke
  window, as are `copy_diagnostics` and `install_update` (26/26).
- SignPath: action `signpath/github-action-submit-signing-request@v3`
  (`f6d0478…`); input `github-artifact-id` comes from
  `actions/upload-artifact`'s `artifact-id`. Artifacts are zips, so the artifact
  configurations use `<zip-file>` roots (`.signpath/`). The Foundation's terms
  require a "Code signing policy" on the homepage with the exact line "Free code
  signing provided by SignPath.io, certificate by SignPath Foundation", team
  roles, a privacy statement, MFA, manual approval per release, and "already
  released in signed form" (🧪 probably means "released"; ask SignPath).
  signpath.org/apply renders its form with JS; field list not seen.

### As built

- **Copy diagnostics** (`diagnostics.rs`): app version, mode, OS, WebView2
  version, pinned image/asset hashes (shortened), key presence (stored/missing
  only), credit, state, the active-instance record, sync report, settings
  summary, catalog status, the state-machine **history** (new: every state-kind
  change and setup-stage change, stamped, max 200), and the log (max 300).
  The whole text goes through `redact` with every secret the app holds (Vast
  key, CivitAI key, current and recorded launch secrets) plus the shape
  patterns, and the home folder becomes `%USERPROFILE%` (case-insensitive,
  either slash, Unicode-safe). Unit tests cover known and unknown secrets, the
  user-name masking, and that useful facts survive. It's on the clipboard, and
  never sent anywhere.
- **Updates:** check 3 s after launch (Vast mode); a banner on the home screen
  (Update now / What's new / Later); **Settings → About and help** shows the
  version and has Check for updates. Install is refused while a session is
  active (in Rust and the UI), because the NSIS installer kills the process
  and would skip the last image sync and the destroy.
- **CI** (`.github/workflows/ci.yml`, $0): Rust fmt/clippy/test and TS build on
  Windows; ruff/mypy/pytest, shellcheck, and the asset-pin check on Ubuntu.
- **Release** (`release.yml`): tag `vX.Y.Z` → draft release; manual dispatch =
  dry run with artifacts only. Version fields must match the tag. Actions are
  pinned by SHA.

### Review fixes (Codex review of PR #7, 2026-09-25)

Verified locally, $0:
- **Tauri updater version binding.** `tauri-plugin-updater` 2.12 has
  `requireSignedVersion`: after checking the minisign signature it requires
  the signature's trusted comment to carry `version:<v>` equal to the version
  `latest.json` announces. CLI 2.11.5 writes it with
  `tauri signer sign --app-version <v>` (checked: trusted comment
  `timestamp:…\tfile:SlopTweak_0.2.0_x64-setup.exe\tversion:0.2.0`). Both on;
  `release_files.py latest-json` refuses an unbound signature.
- **PE version resources.** Both `sloptweak.exe` and the NSIS installer carry
  ProductName `SlopTweak` and ProductVersion `X.Y.Z` (e.g. `0.2.1`), so the
  SignPath configs restrict both to `SlopTweak` / `${version}`. The pinned
  submit action takes `parameters:` as `<name>: "<json string>"` lines.
  ⏳ Whether SignPath accepts the configs is only known once the project is
  approved.
- **curl `-H @-`** reads headers from stdin (checked against a local echo
  server), so provision.sh passes the CivitAI header that way. No header
  file, so nothing to clean up on failure.
- **Deadman** (onstart.sh): a detached loop destroys the instance with
  `CONTAINER_API_KEY` once the sidecar has been unreachable on
  `127.0.0.1:8080` for max(HEARTBEAT_MINUTES, 10) minutes. It covers a failed
  asset fetch or venv setup and a dead sidecar. Simulated against a fake API
  (DELETE sent, key on stdin only); ⏳ not yet seen on a live instance.
- **Asset pin moved** to `ea4bd0bc…` (provision.sh changed). The Phase 1
  pre-release (`instance-v0.1.0`) no longer matches, so the rebuilt bundle is
  published as pre-release `instance-v0.1.1` (2026-09-26; download checked,
  SHA-256 matches the pin) and is the debug builds' default. Whenever the
  bundle changes again, publish a new `instance-v0.1.N` and move
  `DEV_ASSETS_URL`.

Also changed: each create call uses a unique `sloptweak-<nonce>` label, and
an ambiguous create failure (network, parse, 5xx) is checked against the
instance list before giving up. An image that fails to download 5 times stays
missing (quick passes skip it, the final pass retries it) instead of being
dropped. Starting or reattaching a session and installing an update exclude
each other until the installer runs. The session log redacts the stored Vast
and CivitAI keys too, and every known secret is also masked JSON-escaped and
percent-encoded. Mock mode is debug-only.

### Acceptance status (PLAN §4 Phase 5)

| Criterion | Status |
| --- | --- |
| Signed installer installs without a SmartScreen block | ⏳ Blocked on SignPath approval. ⚠️ Known risk: even signed, a new OV certificate builds SmartScreen reputation over downloads, so early installs may still warn. |
| Auto-update works from vN to vN+1 | ✅ locally with release builds (above); ⏳ the real GitHub path needs two published releases |
| Beta users complete a session unassisted | ⏳ the user runs it; kit in `docs/beta.md` |

$0 checks at this point: `cargo test` 127 passed (117 + 10 new); clippy -D warnings clean
(debug and release); mock-ui-check 65/65; webview-check 26/26; sync-check
18/18; instance pytest 25 + release-script tests 4; ruff/mypy clean. No Vast
spend in Phase 5 so far.

## First release build on real Vast (2026-09-26)

The user installed the `v0.2.0` installer from the **draft** release and
pressed Start. A draft's files aren't public:
`releases/download/v0.2.0/instance-assets.tar.gz` returned **404**. So
`onstart.sh` failed at the `curl`, the sidecar never started, no tunnel was
published, and the onstart deadman destroyed each instance ~10 min after
it started (13 min from create). The app retried 3 times (RTX 3070 CA,
RTX 3090 US, RTX 3090 BG; ~$0.10 total, all destroyed) and blamed the host
("the GPU host removed the machine"). Its diagnostics showed `stage: null`
throughout and nothing pointing at the bundle.

Fixed in 0.2.1:
- **Pre-rent asset check** (`assets.rs`): in Vast mode, `start_session`
  downloads the bundle the way `onstart.sh` does and checks
  `ASSETS_SHA256` before renting. It was checked live: the draft URL gives
  "HTTP 404 … release may not be published yet. No GPU was rented.",
  `instance-v0.1.1` passes, and `instance-v0.1.0` fails the hash.
- An instance that the host reported `running` but whose sidecar never
  answered, and which then vanished, is reported as "SlopTweak's setup never
  started on it", not as a host removal.
- A log line (so it shows in diagnostics) once the host has said `running`
  for 4 min with no sidecar answer, saying whether a tunnel was published.
- Mock behaviour `nosidecar` and session tests for both paths.

Lesson for releasing: don't press Start on a draft build (already in
`docs/releasing.md` step 6); the app now refuses at $0 instead.

## Model default settings (Invoke 6.14.1 source, 2026-09-26)

Checked against the `v6.14.1` tag (commit `027be7e2`), then live (below).

- ⚠️ **Invoke doesn't apply a model's `default_settings` when you select
  it.** Only the ✨ **Use default settings** button (`PiSparkleFill`, by the
  main model picker, `UseDefaultSettingsButton.tsx`) dispatches
  `setDefaultSettings`. Its tooltip lists the settings that don't match, and
  it's disabled when everything already matches. `modelSelected`,
  `modelsLoaded` and `appStarted` never read `default_settings`. That's why
  the Phase 6 sessions showed the UI's own 7.5 / 30 for every model.
- Invoke already sets defaults at registration
  (`MainModelDefaultSettings.from_base`, called from
  `ModelConfigFactory`): SDXL gets only 1024×1024, and **Anima gets 35
  steps, CFG 4.5, 1024×1024, the same for Turbo** (Invoke has no Anima Turbo
  variant). So without our override, the button would put Turbo on 35 / 4.5.
- Update: `PATCH /api/v2/models/i/{key}`, body `ModelRecordChanges`
  (`{"default_settings": {...}}`). It **replaces `default_settings` as a
  whole** (`model_records_sql.update_model` copies each field that is set),
  so read the record first and merge. `MainModelDefaultSettings` has
  `extra="forbid"`: `vae, vae_precision, scheduler, steps (>0),
  cfg_scale (≥1), cfg_rescale_multiplier, width, height, guidance,
  cpu_only, fp8_storage`. The body field is a union with the LoRA,
  control-adapter and external-API settings types, and `{width, height}`
  alone would also match the external one, so always send a main-only key
  (cfg/steps/scheduler).
- Find the key with `GET /api/v2/models/?model_type=main` → `{models:
  [...]}`. An in-place install records the absolute path as `path` and
  `source`.
- `scheduler` is Invoke's SD scheduler list (`euler_a` = Euler Ancestral).
  **Anima has its own `animaScheduler` parameter** (default `euler`), so
  `default_settings.scheduler` does nothing for Anima; the catalog rejects
  it there.
- Catalog values: **Banana Splitz** `euler_a` / CFG 5 / 30 steps. The card
  says v3.0+ is "for Euler a sampler only" and gives no CFG or steps. The
  CFG and steps come from its v1.2.1 CivitAI gallery: of 174 images with
  metadata, CFG 3.5–5 and 30 steps are the most common (20 and 14 steps
  also common). **Anima Aesthetic** CFG 4.5 / 35 (card: 30–50 steps,
  CFG 4–5; same as Invoke's own). **Anima Turbo** CFG 1 / 10 (card: CFG 1,
  8–12 steps; 10 is what Phase 6 tuning used).
- As built: the catalog carries `default_settings` (validated in
  `catalog.rs`); `launch_spec` puts it on the main file's `MODELS_B64` entry;
  `instance/model_defaults.py` (stdlib only) merges and PATCHes after
  registration, and a failure is only logged (`model-defaults.log`). Asset pin
  → `16e68e06…`; debug builds point at pre-release `instance-v0.1.2`
  (published 2026-09-26; downloaded back, SHA-256 matches the pin). Older apps ignore the new catalog field, and older
  bundles ignore the new MODELS_B64 key.
- ✅ **Live (2026-09-26, instance 52802661, offer 48529478, RTX 3060 12 GB,
  New Brunswick, $0.0838/hr incl. storage, $0.0039/GB down):** all three
  models on one instance (`launch_dev.py create --model …` ×3; 5 files,
  16.7 GB, shared Anima files once). Image was cached: downloading after
  93 s, Ready at **532 s** (downloads ~50–110 MB/s). `GET /api/v2/models/`
  via the sidecar with the bearer showed, for each main model, the catalog
  values plus Invoke's own 1024×1024 kept: Banana Splitz `euler_a` / 30 /
  CFG 5.0; Anima Aesthetic 35 / 4.5; Anima Turbo 10 / 1.0. ⏳ The ✨ click in
  the UI wasn't done (the agent's permission check blocked reading the login
  ticket); what the button does is from source only. Spend **$0.079**
  (credit $10.7214 → $10.6420). An earlier attempt (52802144) was stopped
  while loading and cost $0.0002.

## Rental log, model default settings

| Instance | Offer | Outcome |
| --- | --- | --- |
| 52802144 | 48529478 (RTX 3060, NB) | Stopped while loading; gone on check |
| 52802661 | 48529478 (RTX 3060, NB) | 3/3 PASS; destroyed by the runner |

## Tutorial v2 groundwork (Invoke 6.14.1 source, 2026-09-26)

Read from `en.json`, `useHotkeyData.ts`, and `useAutoFitBBoxToMasks.ts`
at tag `v6.14.1`. Not yet checked in a live instance.

- **Fit Bbox To Masks**: a canvas toolbar button, hotkey **Shift+B**
  (**Fit Bbox To Layers** is Shift+N). One-time action, not a mode. It fits
  the union of visible inpaint masks, pads by `maskBlur + 8` px, and snaps
  to the bbox grid. For a small mask (eyes) that leaves almost no context,
  so enlarge the bbox by hand afterward.
- **Scale Before Processing**: *Auto* scales the bbox area to the model's
  best size before generating; *Manual* sets Scaled W/H. This is Invoke's
  version of "inpaint only masked at full resolution".
- Denoise controls: **Denoising Strength**; per-mask **Denoise Limit** and
  **Image Noise**; **Optimized Image-to-Image** (Flux only, beta).
  Blending: **Mask Blur**, **Coherence Pass** (Mode, Edge Size, Min
  Denoise).
- **Prompt Templates** (code calls them "style presets"): positive and
  negative prompts with a `{prompt}` placeholder; **Create Prompt
  Template**, **Flatten selected template into current prompt**, import
  from CSV/JSON (columns `name`, `prompt`/`positive_prompt`,
  `negative_prompt`). Not tied to a model. **Trigger Phrases** (Model
  Manager) is a separate insert-by-hand picker.
- Gallery → canvas: **New Canvas from Image → As Raster Layer (Resize)**
  (already used by v1); **Send To Canvas** also exists.

### Label check for the v2 copy (source, tag `v6.14.1` = commit `027be7e`)

Every label and hotkey the Phase 6 copy names, read from `en.json` and the
components at the tag. Still to confirm live during tuning.

| Copy says | Source | Where it is |
| --- | --- | --- |
| **Generate** tab | `ui.tabs.generate` | left tab bar |
| **Width** / **Height** | `parameters.width/height` (`Dimensions*.tsx`) | **Image** accordion (Generate tab); 832×1216 are multiples of 64, so no snapping |
| **Add Negative Prompt** | `common.addNegativePrompt`, `NegativePromptToggleButton.tsx` | ⚠️ the negative box is **hidden by default** (`negativePrompt: null`); a ± icon button on the positive prompt box shows it |
| **Seed**, **Random** | `parameters.seed`, `common.random` | **Image** accordion; Random is a switch |
| Prompt templates | `StylePresetMenuTrigger.tsx`: a bar above the prompt showing **Choose Prompt Template** (`stylePresets.choosePromptTemplate`) with a caret (**View Template List**) | menu has **My Templates** / **Default Templates**, and a **+** button **Create Prompt Template** |
| Template form | **Name**, **Positive Prompt**, **Negative Prompt**, **Insert placeholder**, **Save** | placeholder is literally `{prompt}` (`PRESET_PLACEHOLDER`); without it the template is appended to the end |
| **New Canvas from Image → As Raster Layer (Resize)** | `ContextMenuItemNewCanvasFromImageSubMenu.tsx` | gallery right-click. `withInpaintMask: true`: it resets the canvas, adds the image as a raster layer, then adds an empty **Inpaint Mask** and selects it. "(Resize)" re-uploads the image at the model's optimal area (`calculateNewSize`), as an intermediate |
| **Brush** **B**, **Bbox** **C**, **Eraser** **E**, **Move** **V**, **Undo** Ctrl+Z | `useHotkeyData.ts` | canvas hotkeys |
| **Fit Bbox To Masks** **Shift+B** | `CanvasToolbarFitBboxToMasksButton.tsx` (tooltip `controlLayers.fitBboxToMasks`) | canvas toolbar |
| **Reset Layer** **Shift+C** | `hotkeys.canvas.resetSelected` | clears the selected Inpaint Mask / Regional Guidance |
| **Scale Before Processing**: None / **Auto** / Manual | `BboxScaleMethod.tsx` | Canvas tab → **Image** accordion → **Advanced Options** expander. **Default is Auto** (`scaleMethod: 'auto'`), so stage 4 only has to point at it |
| **Denoising Strength** | `ParamDenoisingStrength.tsx` | top of the canvas **Layers** panel; disabled with a badge when there's no raster content |
| **Opacity** | `EntityListSelectedEntityActionBarOpacity.tsx` | the bar above the layer list; applies to the **selected** layer; 0–100 % number box with a slider dropdown |
| **Add Layer** (+) → **Raster Layer** | `EntityListGlobalActionBarAddLayerMenu.tsx` | bar under the layer list (groups **Regional** / **Layers**) |
| Brush colour | `ToolFillColorPicker.tsx`: **Foreground Color** / **Background Color** swatch (aria **Fill Color**); **D** resets, **X** swaps | canvas toolbar |
| **Accept (Enter)**, **Save To Gallery** | `StagingAreaToolbarAcceptButton.tsx` (Enter hotkey), `…SaveSelectedToGalleryButton.tsx` | staging toolbar under the canvas |

- **Accept doesn't touch the mask** (`StagingArea/context.tsx`): the result
  becomes a new raster layer, the staging session resets, and the Inpaint
  Mask keeps what was painted. Stage 5 therefore tells the user to clear it
  (**Shift+C** with the mask selected) before masking the visor.
- Upload API (`routers/images.py`): any `image/*` content type that PIL
  opens is accepted (415 otherwise), so the fallback can stay **WebP**.

### Tutorial v2 (as built, before live tuning)

- Replaces the v1 vase tour (`tutorial-sample.jpg` and
  `dev/make-tutorial-sample.py` are gone). Same injection and isolation as
  v1 ("Tutorial (as built)" above): plain page JS in an open shadow root,
  no capability, requests only to its own origin.
- A home card (Start / **Continue stage N**, and a list to jump to any
  stage), then stages 1–5 with 3, 3, 2, 6, and 5 cards. Stages 4 and 5
  entered from the home card get one extra first card ("Get the picture on
  the canvas": paste the prompt, **Use ours instead**, open it on the
  canvas), because a new GPU has an empty gallery and canvas.
- **Resume across GPUs:** localStorage is per tunnel origin, so the app keeps
  the stage. New signals, both same-origin navigations that `on_navigation`
  blocks: `/__sloptweak/tutorial/stage/<1-5>` (strict: one digit, nothing
  after it) saves `tutorial_stage` in settings.json, and `…/model-page`
  opens the current model's catalog page in the browser. The page could
  fake either; the worst it can do is set a stage number or open that one
  validated page (https, `civitai.com` / `huggingface.co`, no port or
  userinfo; `Model::page_url`), at most once per 5 s. Skip keeps the stage
  (sets `tutorial_done`); Done sets `tutorial_done` and resets the stage to 1.
- Auto-advance (queue `completed` above a per-card baseline) on the cards
  that end with **Invoke**: 1.2, 4.4, 4.5, 5.4 (numbered from 1).
- Tuned numbers, seeds, and tags are in one `TUNE` object at the top of
  `tutorial.js` (values: "Phase 6 tuning" below).
- `fake_invoke.py --overlay` serves the overlay on every page load and
  answers the signal URLs with 204 (the page stays, as in the app), listing
  them in `/__fake/state`. It also rejects non-`image/*` uploads with 415,
  like Invoke. Checked in the browser pane: all five stages, auto-advance on
  fake generations, the WebP upload (decodes at 832×1216), stage signals
  `stage/2`…`stage/5` then `done`, and a new-GPU resume at stage 5. The
  tallest card is 338 px (fits a 720 px window).

### Tutorial v2 + the v1 feedback round (merge, 2026-09-26)

The Krea 2 branch's v1 tutorial changes were ported into v2 when merging
main (user decision): the home card opens centered over a dimmed page; step
cards and the minimized pill start bottom-right (16 px) and drag by their
title line (position kept on screen, saved in the v2 state); a highlight
ring on steps whose click target is one of the checked 6.14.1 labels
(Generate, Add Negative Prompt, Create/Choose Prompt Template, Invoke, the
canvas menu labels, Inpaint Mask, Scale Before Processing / Advanced
Options, Denoising Strength, Accept, Add Layer, Opacity); and a last
stage-5 card, "Where to go next", with the Invoke docs links
(`/__sloptweak/docs/<key>` → `remote::invoke_docs`). Stage 1 no longer
asks for ✨ since `defaults.js` applies steps/CFG, except for models whose
catalog settings set a scheduler (Banana Splitz): 6.14.1's recall handler
has no scheduler case, so there the card still says to click ✨.
sync-check covers the docs step, the placement, and the centered home card;
⏳ the ring targets and dragging in v2 aren't checked against live Invoke yet.

### Tutorial v2 copy round (user feedback, 2026-09-26)

- Audience: not assumed technical. The copy keeps Invoke's own UI terms
  (bbox, Scaled Bbox, Denoising Strength) so the UI makes sense, but drops
  internals (scheduler, CFG, Scale Before Processing). No talking down:
  "NSFW content", not "adult pictures".
- Prompting advice: match the captions the model was trained on. Templates
  are framed as automating the repeated part (quality tags, standard
  negative), not a second lesson on reading the model page.
- "Use ours instead" is always optional; nothing says the GPU starts empty.
- Regenerating is mentioned twice (stage 1 end, "Keep the best one"), with
  the note that Random must be on or the same seed gives the same picture.
- Eye recolor first try is **0.2** (0.3 turned Banana Splitz's eyes too
  green for the "too weak" demo); the follow-up says the eyes barely change.
- The visor text goes at the **start** of the prompt (earlier words weigh
  more). 0.7 was tuned with it at the end; the user judged it fine without a
  re-tune.
- Stage 5 moves the card to the bottom left (it covered the layer list); any
  other stage drops that position, so a replay starts bottom-right again.
- Checked: all cards in the browser pane via `fake_invoke.py --overlay`;
  sync-check 60/60, mock-ui 65/65, webview 26/26; cargo test 158, clippy clean.

### Closing the Invoke window asks to stop (2026-09-27)

- Closing the Invoke window while a session is active shows a native
  "Stop renting the GPU?" prompt (`ask_stop_on_invoke_close`, lib.rs). The
  remote window has no capabilities, so it's a Rust-side dialog.
- tauri-plugin-dialog 2.7.3 → rfd 0.16 with `common-controls-v6`: a
  **TaskDialog** with Yes/No common buttons and `nDefaultButton: 0`, so the
  first button (**Yes**) is the default (Windows docs). It also allows
  cancellation (Esc, its own X) → `Cancel`; we treat that as "never mind"
  and leave Invoke open. Yes → close + `request_stop`; No → close only.
- `dev/win-dialog.mjs` drives it by window message (WM_CLOSE on the Invoke
  window, `TDM_CLICK_BUTTON` on the dialog). sync-check 64/64 covers Cancel,
  No, and Yes (state `ready`/`ready`/`idle`).

### Codex beta-risk review of v0.2.3 (2026-09-27)

Four points; verdicts after checking the code:

1. **Missing `CONTAINER_ID`/`CONTAINER_API_KEY` → no self-destroy.** Not a
   live risk. The tunnel host reaches the app only through the instance
   label, and `provision.sh` publishes that label with the same credential.
   Without it the app never connects, the attempt fails at the ready
   timeout, and the app destroys it with the user's key. Vast has always
   set both (§2). Not changed (it would need a new asset bundle and a paid
   run).
2. **Ambiguous create found late → untracked until restart.** Valid. After
   `find_created` gives up (~15 s), `destroy_late_create` keeps polling for
   the attempt's unique label for 10 min and destroys the instance if it
   appears. It matches only that label, so a friend's session on a shared
   account is never touched. Test: `late_create_is_destroyed_in_the_background`
   (mock `LateCreate`).
3. **Output folder changed mid-session → earlier images look missing.**
   Valid (UX). The folder is now locked while a session is active:
   `pick_output_folder`/`reset_output_folder` refuse, and Settings disables
   the buttons with a note. sync-check covers it.
4. **Unsigned installer.** Known; SignPath is pending. Until then, send the
   release link and the SHA-256 from the release notes through a trusted
   channel.

### Phase 6 tuning (live, 2026-09-26) ✅

Three held sessions (`dev/phase6-session.mjs`), one per catalog model; all
on an **RTX 5060 Ti 16 GB** (Blackwell, cc 12.0, Connecticut, $0.1559/hr),
which the app picked as the cheapest offer. **Blackwell works** with the
pinned Invoke 6.14.1 image (first time on this GPU generation). Each stage
was done once through the real UI over CDP (label check + graph capture),
then varied by replaying the captured graph through
`POST /api/v1/queue/default/enqueue_batch` with edited seed / denoise /
crop / mask (`dev/phase6-lib.mjs`). Outputs in `.dev/phase6/` (not
committed).

**Live UI checks** (Banana Splitz and Anima Aesthetic): every label the copy
names is on screen: Generate, Width, Height, Seed, Random, Add Negative
Prompt (± on the prompt box; the negative box then appears), Choose Prompt
Template, View Template List, Assets, New Canvas from Image → As Raster
Layer (Resize) (`[role=menuitem]`), Inpaint Mask, Raster Layer, Denoising
Strength, Opacity, Advanced Options, Scale Before Processing (**Auto** by
default), Fit Bbox To Masks, Add Layer, Fill Color, Accept (Enter), Save To
Gallery, Discard All, CFG Scale, Steps. **Shift+B** fits both eye masks
(Bbox 264×144 → Scaled Bbox 1392×760); **Shift+C** clears the selected
mask. New live facts:

- The canvas's top-left corner shows **Bbox: W×H px / Scaled Bbox: W×H px**,
  the visible proof of Scale Before Processing. Stage 4 points at it.
- **While tries are staged, the canvas can't be painted** (brush, Shift+C do
  nothing) and Invoke re-runs the old bbox. Stage 5 says to Accept first.
- A bbox with no raster content under it makes a plain txt2img graph and
  disables Denoising Strength.
- Tight bbox on one eye at the default 0.75: the model drew a whole tiny
  woman inside the eye. The "the bbox is what the model sees" lesson, live.
- "(Resize)" on Anima makes the layer 848×1240 (its optimal area), not
  832×1216.
- Invoke's defaults are **CFG 7.5, 30 steps** for every model: Anima
  Aesthetic comes out oversaturated (its card says CFG 4–5), and Turbo
  looks burned (card: CFG 1, 8–12 steps).

**Results** (seeds 1/2/3 for the inpaints; our portrait unless noted):

| | Banana Splitz XXL | Anima Aesthetic | Anima Turbo (CFG 1, 10 steps) |
| --- | --- | --- | --- |
| Stage 1, 8 seeds at 832×1216 | all usable, SFW; **42** clean front view | all usable, SFW (flat, oversaturated at CFG 7.5); **42** | defaults burned; at CFG 1 three of four seeds carry a watermark/signature; **123** clean |
| Eyes at 0.3 | muddy olive, broken pupils stay (own seed-42 image: still red) | still red | still red |
| Eyes at 0.55 | clean green, artifacts fixed | green | olive; **0.65** green (0.7: one seed winks) |
| Visor, 50 % blue band painted | clear see-through blue visor at 0.6–0.75, cleanest **0.7** | blue visor at all; flat at 0.6, glassier 0.7–0.75 | visor at 0.7–0.75 |
| Visor, no paint, 0.7 | thin clear glasses / face lines, no blue visor | nothing / small cyan strip | nothing / faint gray strip |
| Template (page tags) | not tried live | visibly better: less oversaturated, real shading | — |

- At **CFG 1 the negative prompt does nothing** (no negative guidance), so
  `nsfw` can't keep Turbo safe. The Turbo copy adds `safe,` to the prompt
  (Anima's own safety tag); all Turbo tests were SFW.
- Chosen: seed 42 / 42 / 123; eye denoise 0.3 then 0.55 (Turbo 0.65);
  visor band layer at 50 % opacity, denoise 0.7 (0.6–0.75). The plan's
  alternative (lower the accepted result's opacity) isn't needed.
- Harness notes: model keys are per install (a graph's `vae_model` /
  `qwen3_encoder_model` keys from one instance fail with
  `UnknownModelException` on another); the queue helper must count
  `failed` too or it waits forever.

### Phase 6 rental log (all destroyed; none running)

| Instance | Model | Ready in | Held | Cost |
| --- | --- | --- | --- | --- |
| 52786625 | Banana Splitz XXL | 356 s (cold) | 23.5 min | ~$0.069 |
| 52790099 | Anima Aesthetic | 163 s | 20 min | ~$0.063 |
| 52793084 | Anima Turbo | 218 s | 21.7 min | ~$0.079 |

Phase 6 total **~$0.21** (credit $10.9484 → $10.7341; late charges may post).

### Tutorial v2 fallback asset

`app/src-tauri/assets/tutorial-portrait.webp` (832×1216, lossy VP8). The
user's file carried only an sRGB ICC chunk (Google 2016, no EXIF/XMP); it
was rewritten as a plain `RIFF/WEBP/VP8 ` container with the same bitstream
(pixels identical, 49,480 bytes). Uploaded as `image/webp`.

### Re-tune at the catalog settings (live, 2026-09-26) ✅

The Phase 6 numbers for Banana Splitz and Anima Aesthetic were tuned at
Invoke's generic CFG 7.5 / 30 steps. Stage 1 now has the user click ✨, so
they were re-checked at the catalog settings: Banana Splitz Euler a / CFG 5
/ 30, Aesthetic CFG 4.5 / 35 (Anima scheduler unchanged, `euler`). Turbo
was already tuned at CFG 1 / 10.

- **How:** one instance with both models (`launch_dev.py create --model`
  ×2), then the saved Phase 6 graphs (`graph-generate.json`,
  `graph-inpaint.json`) replayed through the sidecar with the bearer:
  model keys and hashes remapped to the new instance, the saved eye and
  visor crops and masks re-uploaded, settings, seed, prompt and denoise
  edited, then `POST /api/v1/queue/default/enqueue_batch` with
  `{"batch": {"graph", "runs": 1}, "prepend": false}`, polling
  `GET /api/v1/queue/default/i/{id}`. 44 images, no UI. The script was a
  one-off in the agent's scratchpad (not committed).
- **Instance 52807964**, offer 43619154, RTX 5060 Ti 16 GB, British
  Columbia, $0.1543/hr incl. storage, $0.0026/GB down. The first choice
  (offer 48529478) was gone at create time, and nothing was rented then.
  Ready in **609 s** (cold). Banana Splitz ~13 s per 832×1216 image; Anima
  Aesthetic ~35 s per image. Spend **$0.086** (credit $10.6420 →
  $10.5557).

| | Banana Splitz (Euler a, CFG 5, 30) | Anima Aesthetic (CFG 4.5, 35) |
| --- | --- | --- |
| Stage 1, 8 seeds at 832×1216 | all usable, SFW; **42** still a clean front view | all usable, SFW, less flat and oversaturated than at 7.5; **42** clean front view |
| Eyes at 0.3 | **changed:** yellowish green, broken pupils fixed (at 7.5: muddy olive, pupils broken) | still red |
| Eyes at 0.45 | green | red to brownish |
| Eyes at 0.55 | clean green | green (seed 1 clean) |
| Eyes at 0.65 | green | green |
| Visor, 50 % band | see-through visor at 0.6–0.75, **0.7** clean on both seeds | flat at 0.6, glassier at **0.7**–0.75 |

Kept: seeds 42 / 42 / 123, eyes 0.3 → 0.55 (Turbo 0.65), visor 0.7. The
only copy change is what 0.3 does, now per model (`TUNE.eyeLowResult`).

### Re-tune rental log (all destroyed; none running)

| Instance | Offer | Outcome |
| --- | --- | --- |
| — | 48529478 (RTX 3060, NB) | Create refused (offer gone); nothing rented |
| 52807964 | 43619154 (RTX 5060 Ti, BC) | 44/44 images; destroyed by the runner |

## Release 0.2.2 check (2026-09-26) ✅

`releasing.md` step 2 with the 0.2.2 debug build (bundle `instance-v0.1.2`,
pin `16e68e06…`): `TEST_MODEL=banana-splitz-xxl GENERATE=1 MAX_DPH=0.25
MAX_SESSION_MINUTES=30 node dev/vast-acceptance.mjs r1` (the two caps are
new env overrides). The app picked offer 38666258, RTX 3090, Bulgaria,
$0.2106/hr + $0.018 download, instance 52819603. Ready in **162 s** (image
cached); one image in 15 s; Invoke in the app window, remote IPC denied,
model registered, cost title, Stop destroys, record cleared, machine
remembered as good: **11/11**. Spend ~$0.025 (credit $10.5524 → $10.5273).
$0 checks at the same commit: cargo test 144, clippy, `npm run build`,
mock-ui-check 65/65, webview-check 26/26, sync-check 50/50, instance
pytest 33, ruff/mypy, `build_assets.py --check-pin`.

## Release 0.2.3 check (2026-09-26) ✅

Krea 2 / Kroma catalog, automatic model defaults, saved templates and
workflows, and the tutorial copy round (PR #12). `instance/` is unchanged
(bundle pin `16e68e06…`, `--check-pin` matches), so `releasing.md` step 2's
real-Vast run was not repeated; PR #12's live runs cover the app changes.
$0 checks: cargo test 158, clippy, `npm run build`, mock-ui-check 65/65,
webview-check 26/26, sync-check 60/60, instance pytest 33, ruff/mypy.

## Release 0.2.4 check (2026-09-27) ✅

The close-Invoke prompt and the beta-risk review fixes (PR #14).
`instance/` is unchanged (pin `16e68e06…`, `--check-pin` matches), so no
real-Vast run. $0 checks at PR #14's head (the code on `main`): cargo test
159, clippy, typecheck, sync-check 65/65, mock-ui-check 65/65,
webview-check 26/26.

## Still open

- ~~Live upstream catalog edit~~ **done (2026-09-25).** Merging PR #3
  (Anima) changed `catalog.json` on `main`. The next launch logged
  `catalog: 3 models from https://raw.githubusercontent.com/…/main/…` and
  showed all three models ("updated just now"); earlier the same fetch
  returned 1. The "no rebuild needed" part is covered by mock-ui-check (an
  upstream edit shows up in a running app without a rebuild), because the
  live build also bundles the new catalog.
- ~~Wizard screenshots~~ **shipping without them (user decision,
  2026-09-25).** Still possible later: Wizard screenshots of the logged-in
  Vast/CivitAI pages. The Chrome profile
  is signed in now (2026-09-25), but Claude in Chrome's screenshots here only
  come back to the agent; `save_to_disk` wrote no file anywhere findable, so
  nothing could be cropped/blurred into the repo. Drop PNGs into
  `app/src/wizard/` (`vast-signup`, `vast-billing`, `vast-keys`,
  `civitai-signup`, `civitai-keys`) by hand. Blur the credit, card digits,
  email, and any key.
- Tutorial copy was checked against Invoke 6.14.1. A future image bump must
  re-check the labels (`tutorial.js` header) and the queue/image API shapes
  (`sync.rs`).
- Phase 6: tuned and label-checked live (above). Not done: a full
  click-through of all five stages by a person on a clean install (PLAN
  acceptance), and the Banana Splitz template live. Dev scripts that use the
  main window's CDP port need the installed SlopTweak closed (same WebView2
  profile, so the debug port is ignored).
- ~~Invoke's per-model defaults don't match the models~~ **done
  (2026-09-26, PR #9):** the catalog's `default_settings` are in each
  model's Invoke config, and stage 1 has the user click ✨ to load them. The
  tutorial numbers were re-tuned at those settings (below).
