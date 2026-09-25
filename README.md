# SlopTweak

SlopTweak is a free, open-source Windows app for making and editing images with
open-weight AI models on a rented GPU, without a terminal, Docker, or cloud
console.

You bring your own [Vast.ai](https://vast.ai) account (you pay Vast directly
for GPU time, usually about $0.10 to $0.15 an hour) and, for some models, your
own [CivitAI](https://civitai.com) key. SlopTweak rents the GPU, starts
[InvokeAI](https://github.com/invoke-ai/InvokeAI) on it, shows it in a window,
copies every image to your PC, and shuts the GPU down when you're done.

![SlopTweak home screen](docs/images/h1-home.png)

## Download

**[Latest release](https://github.com/ljohnsoncpu/SlopTweak/releases/latest)**:
download `SlopTweak_<version>_x64-setup.exe` and run it. It installs for your
Windows user only; no administrator rights are needed.

- Windows 10 or 11, 64-bit. The app uses Microsoft Edge WebView2, which
  Windows 11 already has; the installer adds it on Windows 10 if needed.
- A Vast.ai account with at least $5 of credit, and a CivitAI account (free)
  for models hosted there.
- Each release lists SHA-256 checksums for its files.

Early releases may not be code-signed yet, so Windows SmartScreen can say
"Windows protected your PC". Click **More info**, then **Run anyway**. See
[Code signing policy](#code-signing-policy).

## What it does

- **Guided setup.** A wizard walks you through creating the Vast account, adding
  credit, and making the two API keys. Each key is checked as soon as you paste
  it.
- **One button to start.** Pick a model and press **Start**. SlopTweak picks the
  cheapest suitable GPU, sets it up, and opens InvokeAI when it's ready
  (usually 3 to 10 minutes; a slow host can take up to 15).
- **Running cost always visible**: price per hour, time, spend so far, and
  credit left.
- **Your images stay yours.** Every image is copied to `Pictures\SlopTweak` while
  the GPU runs, and once more before it shuts down.
- **No surprise bills.** **Stop** or closing the app shuts the GPU down. If your
  PC crashes, sleeps, or goes offline, the GPU shuts itself down within about 10
  minutes, and after 20 idle minutes or 4 hours in any case (you can change
  these). Next launch, SlopTweak finds anything left over and offers to shut it
  down.
- **Private.** The InvokeAI page on the GPU needs a secret only your copy of the
  app has. Keys are stored in Windows Credential Manager, never in files.
- **Built-in tutorial** for inpainting (repainting part of an image), and
  **LoRAs** from a CivitAI link.
- **Automatic updates** from this repository's releases, verified with a
  signature before they install.

Read the **[user guide](docs/user-guide.md)** and **[what it costs and how
billing works](docs/costs.md)**.

## How it works

```
Your PC: SlopTweak (Tauri app)  ──HTTPS──►  Rented Vast.ai GPU
  • rents / stops the GPU via the Vast API    • official InvokeAI image (unmodified)
  • keys in Windows Credential Manager        • sidecar: login secret, heartbeat,
  • copies images to your Pictures folder       self-shutdown watchdog
  • InvokeAI shown in an isolated window      • Cloudflare quick tunnel (HTTPS)
```

The instance-side scripts (`instance/`) are published with each release and
pinned by SHA-256 in the app. Design notes: [PLAN.md](PLAN.md) and
[docs/findings.md](docs/findings.md).

## Privacy

SlopTweak has no server and collects no telemetry. It connects only to:

| Where | Why | What it sends |
| --- | --- | --- |
| `console.vast.ai` | Find, rent, watch, and shut down GPUs; read your credit | Your Vast API key |
| `civitai.com` | Check your CivitAI key; look up LoRA links you add | Your CivitAI key (only for the key check and gated files) |
| `raw.githubusercontent.com` | Download the model list | Nothing personal |
| `github.com` (this repo's releases) | Check for and download updates | Nothing personal |
| Your rented GPU, via `*.trycloudflare.com` | Show InvokeAI; copy your images; keep the GPU alive | A per-session secret |

Your CivitAI key is passed to the rented GPU so it can download models; the
machine's host could in principle read it. The wizard says so. You can
revoke the key on CivitAI at any time. Vast.ai, CivitAI, and Cloudflare have
their own privacy policies and terms. "Copy diagnostics" in the app puts a
report on your clipboard for you to paste into a bug report; it removes keys,
tokens, and your Windows user name, and it's never sent automatically.

This program will not transfer any information to other networked systems
unless specifically requested by the user or the person installing or
operating it.

## Code signing policy

Free code signing provided by [SignPath.io](https://about.signpath.io),
certificate by [SignPath Foundation](https://signpath.org).
*(Application pending: releases before approval are unsigned.)*

- Release binaries are built from this repository by GitHub Actions
  ([`.github/workflows/release.yml`](.github/workflows/release.yml)) and signed
  only through SignPath. Every signing request is approved by hand.
- Only the app (`sloptweak.exe`) and its installer are signed. Third-party
  components (InvokeAI, cloudflared) run on the rented GPU, not on your PC, and
  aren't signed or shipped by this project.

Team roles:

| Role | Members |
| --- | --- |
| Authors (commit access) | [@ljohnsoncpu](https://github.com/ljohnsoncpu) |
| Reviewers | [@ljohnsoncpu](https://github.com/ljohnsoncpu) |
| Approvers (approve signing) | [@ljohnsoncpu](https://github.com/ljohnsoncpu) |

## Building from source

See [app/README.md](app/README.md) (launcher) and [instance/](instance/)
(GPU-side scripts). Releases: [docs/releasing.md](docs/releasing.md).

## License

[MIT](LICENSE). SlopTweak isn't affiliated with Vast.ai, CivitAI, Invoke, or
Cloudflare. Models you download have their own licenses, shown in the app.
