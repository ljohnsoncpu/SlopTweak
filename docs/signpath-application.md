# SignPath Foundation application: draft answers

For the maintainer to paste into <https://signpath.org/apply>. The form itself
loads with JavaScript and couldn't be read ahead of time, so the answers below
follow the Foundation's published conditions (<https://signpath.org/terms>).
Adjust them to the actual fields. **Apply only after** the first release
(v0.2.0) is published and 2FA is on for the GitHub account.

## Checklist against the conditions

| Condition | Status |
| --- | --- |
| OSI-approved licence, no commercial dual-licensing | MIT ([LICENSE](../LICENSE)) |
| No proprietary components | App: Tauri, Rust crates, TypeScript, all open source. The GPU side uses the official InvokeAI image (Apache-2.0) and cloudflared (Apache-2.0), downloaded at runtime and not shipped by us |
| Actively maintained | Commits and PRs through Sept 2026 |
| Already released | v0.2.0 on GitHub Releases (unsigned) — **to do** |
| Functionality described on the download page | [README](../README.md) plus [user guide](user-guide.md) |
| No malware, PUPs, or hacking tools | A launcher for InvokeAI on the user's own Vast.ai account |
| MFA for SignPath and the source repository | GitHub 2FA — **maintainer to confirm** |
| Roles: authors, reviewers, approvers | README → Code signing policy |
| Code signing policy on the homepage, with the SignPath text and the privacy statement | README → Code signing policy and Privacy |
| Built from source in a verifiable way; manual approval per release | GitHub Actions on GitHub-hosted runners ([release.yml](../.github/workflows/release.yml)); SignPath policy with manual approval |

⚠️ The terms page says "Already released in signed form". That probably means a
public release must exist, but it's worth asking SignPath if the form doesn't
make it clear. An unsigned v0.2.0 is all we can offer without paying.

## Answers

**Project name:** SlopTweak

**Repository:** https://github.com/ljohnsoncpu/SlopTweak

**Homepage / download page:** https://github.com/ljohnsoncpu/SlopTweak#readme
(releases: https://github.com/ljohnsoncpu/SlopTweak/releases)

**Licence:** MIT

**Short description:**
SlopTweak is a Windows desktop app that lets non-technical people use
open-weight image models (via InvokeAI) on a GPU rented from their own Vast.ai
account. It rents the GPU, starts InvokeAI, shows it in an isolated window,
copies the user's images to their PC, and destroys the GPU when they're done or
when the PC goes away, so they aren't billed for forgotten machines.

**What will be signed:**
`sloptweak.exe` (the Tauri app, Rust + TypeScript) and its NSIS installer
`SlopTweak_<version>_x64-setup.exe`. Nothing third-party is signed.

**Build system:** GitHub Actions, GitHub-hosted `windows-2025` runners. A tag
push builds the exe, submits it to SignPath, bundles the signed exe into the
NSIS installer, submits the installer, then attaches both to a draft GitHub
release. Actions are pinned by commit SHA.

**Why signing matters for this project:**
The users are non-technical. An unsigned installer gets a SmartScreen "Windows
protected your PC" block and antivirus suspicion, which is exactly where these
users give up. The app handles cloud-billing API keys, so users should be able
to check who published the binary.

**Team / roles:** a single maintainer, GitHub @ljohnsoncpu, is author, reviewer,
and approver. Outside contributions come through pull requests reviewed by the
maintainer before merge.

**Privacy:** No telemetry and no project-run server. The app contacts only
Vast.ai (the user's key), CivitAI (the user's key), GitHub (model list and
updates), and the user's own rented GPU. See the README → Privacy.

**Downloads / users so far:** new project; a small beta with non-technical users
is starting.

**Release cadence:** occasional, a few releases a year after the beta settles;
each signing request is approved by hand.
