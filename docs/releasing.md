# Releasing SlopTweak (maintainers)

Releases are built by [`.github/workflows/release.yml`](../.github/workflows/release.yml)
on GitHub-hosted Windows runners. Pushing a tag makes a **draft** release; a
person publishes it. Nothing is built or signed on a developer PC.

## What a release contains

| File | What |
| --- | --- |
| `SlopTweak_X.Y.Z_x64-setup.exe` | NSIS installer (per-user, no admin). Code-signed once SignPath is set up. |
| `SlopTweak_X.Y.Z_x64-setup.exe.sig` | Updater (minisign) signature of the installer |
| `latest.json` | Updater manifest; the app reads `releases/latest/download/latest.json` |
| `instance-assets.tar.gz` | provision.sh, sidecar.py, requirements.txt for the GPU side. The app pins it at `releases/download/vX.Y.Z/instance-assets.tar.gz` with `ASSETS_SHA256` (config.rs) |
| `SHA256SUMS.txt` | SHA-256 of the installer and the asset bundle |

## One-time setup

**Updater key** (free, separate from code signing). The public key is in
`app/src-tauri/tauri.conf.json` (`plugins.updater.pubkey`). The private key and
its password live only in:

- repository secrets `TAURI_SIGNING_PRIVATE_KEY` and
  `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`, and
- the maintainer's offline backup (password manager).

If the private key is lost, installed copies can't auto-update any more: users
would have to download the next version by hand, and a new key would be needed.
Never commit it, paste it into an issue, or print it in a workflow log.

**Code signing** (SignPath Foundation, after approval): see
[`.signpath/README.md`](../.signpath/README.md). Until the variable
`SIGNPATH_ORGANIZATION_ID` is set, the workflow builds unsigned releases and
says so in the release notes.

## Cutting a release

1. Pick the version `X.Y.Z` and set it in all three places (CI checks they match
   the tag): `app/package.json` (then run `npm install` so the lockfile
   matches), `app/src-tauri/Cargo.toml` (then `cargo check` for `Cargo.lock`), and
   `app/src-tauri/tauri.conf.json`.
2. If anything in `instance/` changed (`provision.sh`, `sidecar.py`,
   `requirements.txt`): run `python instance/build_assets.py` and put the new hash
   in `ASSETS_SHA256` in `config.rs`. CI rebuilds the bundle and fails the release
   if the hash doesn't match the pin. Then run a real-Vast acceptance with a debug
   build, pointing `SLOPTWEAK_DEV_ASSETS_URL` at a test upload of the new bundle
   (debug builds use the `instance-v0.1.0` pre-release by default, because
   `vX.Y.Z` doesn't exist yet).
3. Merge to `main` with CI green. Optionally run the **Release** workflow by hand
   (Actions → Release → Run workflow) for a dry run: it builds and signs
   everything and keeps the files as a workflow artifact, with no release.
4. Tag and push: `git tag vX.Y.Z && git push origin vX.Y.Z`.
5. If code signing is on, approve the two SignPath signing requests (the exe,
   then the installer) in SignPath.
6. Check the draft release: install the installer on a clean Windows user, and
   check that the app starts, the version in Settings → About is right, and
   Windows shows the expected publisher (Properties → Digital Signatures).
   Don't press Start yet: a draft's files aren't public, so the GPU can't fetch
   `instance-assets.tar.gz` until the release is published.
7. **Publish** the draft as a normal release, **not a pre-release**:
   `releases/latest` (and therefore auto-update) skips pre-releases and drafts.
   Installed copies see the update on their next launch.
8. Right after publishing, run one real session with the installed release build
   (Start → one image → Stop, about $0.05). This is the first time a release
   build fetches its own asset bundle from GitHub.

To pull a bad release, un-publish it (back to draft) or publish a fixed
`X.Y.Z+1`. `latest` then points at the previous or the new release.

## Checks before tagging ($0)

```bash
cd app/src-tauri && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
cd app && npm run build
cd app && node dev/mock-ui-check.mjs && node dev/webview-check.mjs && node dev/sync-check.mjs
cd instance && uv run ruff check . && uv run mypy && uv run python -m pytest -q
python instance/build_assets.py --check-pin
```

## Local updater test (no GitHub release needed)

This is how 0.2.0 → 0.2.1 was verified (see findings, Phase 5). Build two
release installers with a test endpoint, sign the newer one, and serve it:

```bash
CFG='{"version":"0.2.0","plugins":{"updater":{"endpoints":["http://127.0.0.1:8765/latest.json"],"dangerousInsecureTransportProtocol":true}}}'
npx tauri build --no-bundle --config "$CFG" && npx tauri bundle --bundles nsis --config "$CFG"
# repeat with "0.2.1", then:
npx tauri signer sign -f <key file> SlopTweak_0.2.1_x64-setup.exe
python .github/scripts/release_files.py latest-json --version 0.2.1 \
  --installer SlopTweak_0.2.1_x64-setup.exe --signature SlopTweak_0.2.1_x64-setup.exe.sig \
  --base-url http://127.0.0.1:8765 --out latest.json
python -m http.server 8765 --bind 127.0.0.1
```

Install 0.2.0 (from PowerShell: `Start-Process <setup.exe> -ArgumentList /S -Wait`;
Git Bash turns `/S` into a path), start it, press **Update now**, and check that
`%LOCALAPPDATA%\SlopTweak\sloptweak.exe` is 0.2.1 afterwards. Uninstall with
`%LOCALAPPDATA%\SlopTweak\uninstall.exe /S`: the test builds point at localhost.
