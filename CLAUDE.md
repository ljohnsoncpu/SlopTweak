# SlopTweak — Agent Conventions

SlopTweak is the product name. The repo folder is still `invoke-launcher`.

## Read first
- `PLAN.md` is the canonical design. Decisions there are settled; flag
  concerns in the PR description or in `docs/findings.md` instead of
  silently diverging.
- `docs/findings.md` holds verified facts about Vast, InvokeAI, cloudflared,
  and Tauri. Prefer it over memory, and update it when you verify something
  new.
- Work phase by phase. Don't start a phase until the previous one's
  acceptance criteria pass and the user has said to proceed.

## Repo layout
See PLAN.md §3: `app/` (Tauri: Rust in `src-tauri/`, TS UI in `src/`),
`instance/` (provision.sh, sidecar.py, tests), `catalog/`, `docs/`.

## Money and real infrastructure
- **Never create a Vast instance, API key, or any billable resource without
  explicit user approval in chat, per action.** State the GPU, $/hr, expected
  duration, and cap.
- Any script that creates an instance must destroy it on every exit path
  (trap/finally) and print the instance id first.
- Use `MockProvider` for UI and state-machine work. Real-Vast tests stay
  behind a manual trigger.

## Secrets
- Vast/CivitAI keys only in Windows Credential Manager (app) or env vars
  (dev scripts). Never on disk, in logs, URLs, test fixtures, or commits.
- Redact keys and tokens in any output you print or paste into docs.
- Instance-side credentials must be least-privilege (see PLAN §9.1).

## Code conventions
- Rust: `cargo fmt`, `cargo clippy -- -D warnings`, and unit tests for
  provider, state machine, offer filtering, and redaction. All Vast calls go
  through the `GpuProvider` trait.
- Python (instance/): ruff + mypy strict; pytest with an injectable clock and
  a mocked Vast API. The sidecar runs in its own venv
  (`uv venv /opt/sidecar-venv`; `uv` ships in the Invoke image) with pinned
  deps (aiohttp). Never import from or install into Invoke's `/opt/venv`.
- Shell (provision.sh): `set -euo pipefail`, shellcheck-clean, LF line
  endings. `onstart` itself stays under 4048 chars.
- TS: plain TS or Svelte, strict mode.
- Pin versions: the InvokeAI image by `vX.Y.Z-cuda@sha256:…` (tags have a
  `-cuda` suffix; ≥ v6.13.8), the cloudflared version, and instance-asset
  SHA-256s. Check a tag exists in the registry before using it.

## Security invariants (don't regress)
- Invoke binds 127.0.0.1 (`INVOKEAI_HOST=127.0.0.1`); only the sidecar port
  is exposed.
- cloudflared always runs with `--metrics 127.0.0.1:<port>`.
- The remote webview window has no Tauri capabilities and a dedicated data
  directory.
- Launch token: 256-bit, single-use, 60 s; only its hash goes in instance env.

## Ask, don't assume
- Anything in PLAN §9 (watchdog credential, transport, content policy,
  models, naming, signing).
- IAM-like scoping of Vast keys, and anything that spends money.

## Git
- Commit only when asked. Branch off `main` for feature work.
- Never commit `.env`, keys, or rental logs containing unredacted output.
