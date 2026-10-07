# Focus-on-a-part live A/B: run sheet

Status: **code written and tested against the mock ($0). Nothing has been rented;
waiting for your go.** Written 2026-10-05.
Background: `docs/findings.md`, "Focus on a part of a character sheet".

## Question
Does sending only a painted part of a character sheet (the face, plus a little
context) hold the character's identity better than sending the whole sheet?
Secondary: does enlarging a small crop (under 0.25 MP, to 0.5 MP) help or just
soften it? (See the matrix notes: this run can't separate that one.)

Nothing instance-side changes (the crop is an `ImageCrop` node in the graph the
app builds), so no new instance bundle is needed; the existing pinned one is used.

## Money (needs your go, per CLAUDE.md)
- **GPU:** whatever the app picks for `wulver-identity-edit` at the price limit:
  the cheapest offer meeting the model's floors (32 GB+). The last two runs got an
  RTX 4080S 32 GB at $0.60/hr. **Not re-queried today:** the app chooses at Start and
  the script prints the offer the moment the instance exists. The price limit
  ($0.75/hr) is enforced by the app before it rents.
- **Phases and cost at about $0.60/hr:**

  | Phase | Time | Cost |
  | --- | --- | --- |
  | Start to Ready (download ~33 GB) | 12 to 17 min (timeout 30) | $0.12 to $0.17 |
  | A/B: 14 generations plus painting | about 25 min | about $0.25 |
  | **Hold for you** (see below) | until you press Stop, or a backstop fires | $0.60/hr |

- **Backstops while it is held** (they run on the instance and need nothing from
  me): destroy after **60 min idle** (no POST to the GPU; polling doesn't count),
  after **180 min since the sidecar started** (so about 2 h 40 min of use after a
  15 min start), or **10 min after the app stops sending heartbeats**.
- **Worst case:** 180 min at $0.75 = **$2.25**, start-up included. Typical if you use
  it for an hour afterwards: about $0.85 total. Say if you want different idle or
  session numbers (idle 5 to 240 min, session 30 min to 24 h are the app's limits).
- Before the A/B finishes there is also a **hard stop: 75 min after launch** that
  destroys the instance (start-up can take up to 30 min, then about 25 min of images;
  this stops a stuck run billing). It is cleared at hand-off. A run that fails
  **before the GPU is ready** is destroyed, not kept.

## Leaving it up: the exception to "destroy on every exit path"
CLAUDE.md says an instance-creating script destroys it on every exit path. You asked
for this one to stay up, so this run is an explicit, per-run exception, and the
safety moves from the script to the instance's own watchdog (above). With `KEEP=1`
(the default) the script:
- does **not** click Stop, does not run the "safety net" delete, and does not kill the
  app or vite;
- launches the app **detached with its window visible**, and vite too (the debug app
  loads its UI from `localhost:1420`), and leaves a small catalog server running (the
  app's catalog URL points at the script's);
- exits after printing the instance id, the offer, when each backstop fires, and how
  to stop it;
- leaves the run's `settings.json` in place (the live session was created with those
  values) with your original beside it. After you stop the GPU,
  `node dev/focus-live.mjs --restore-settings` puts your settings back.

What keeps it alive: this PC awake and the SlopTweak window open. If the PC sleeps or
the app is closed for more than 10 minutes, the instance destroys itself (the safe
direction). If only the app crashes, relaunching it shows the orphan banner with
**Reconnect**, as in the 2026-10-03 run. Leaving the app up also leaves its local
debug port (9334, localhost only) and vite (1420) open, as in every dev run.

If the Claude session ends and takes its child processes with it, the app dies and the
GPU is destroyed 10 min later. That loses the machine but cannot leak money. Tell me if
you'd rather I launch the app through the Terminal pane so it outlives the session.

You stop it with the app's **Stop** (saves what's left, then destroys). Vast's
instances page also works: <https://cloud.vast.ai/instances/>.

## Inputs (provided; in `C:\Users\user\Documents\sloptweak-focus-ab\`, outside the repo)
The three pictures you attached, copied as `inputs\fox.webp` (kimono fox, 1024x1536),
`inputs\tiger.webp` (five-view tiger sheet, 1359x2000) and `inputs\husky.webp`
(husky in a suit, 832x1216), plus `config.json` with their sizes and a face box each
(fractions of the picture; I checked the boxes by eye against the images):

| Picture | Face box (x0, y0, x1, y1) | Role |
| --- | --- | --- |
| fox | 0.33, 0.012, 0.575, 0.215 | sheet in cases A, B |
| tiger | 0.095, 0.05, 0.245, 0.135 (the small front-view head; the sheet also has a large headshot, which is *not* used) | sheet in cases B, C |
| husky | none | the picture to edit in case C (swap a new face onto it) |

`OUT` (the results) is `C:\Users\user\Documents\sloptweak-focus-ab\out-live\`.

## What was built
1. **Fixed seed (debug builds only):** `SLOPTWEAK_DEV_SEED_FILE` names a file whose
   number is the seed for the next image (`lib.rs`, unit-tested). Release builds
   ignore it.
2. **`app/dev/focus-live.mjs`**, derived from `edit-live.mjs`: same Vast helpers and
   guards, a pair runner, face painting with real mouse events, blind file naming,
   `review.html`, `KEEP`, `MOCK`, `--restore-settings`.
3. **Debug-only app log lines** (`[dev-focus] ...`): the app writes what it was asked
   (seed, painted boxes) and any refusal to its log, which the script reads. They are
   not printed anywhere I look.
4. **A real bug found on the way, fixed:** the panel hid every refused request,
   because an error was cleared by the refresh that followed it. `identityAct` in
   `main.ts` now shows the error after the refresh.
5. **Tested for $0:** the whole script against MockProvider (14 images, 14/14 checks),
   the `KEEP` hand-off (app, vite and the catalog server all survived the script
   exiting), 205 Rust unit tests, clippy, `tsc`, and the mock UI check (46/46). What
   the mock can't tell us: whether the images are any good, and the real GPU's timing.

## How to run (after you say go)
```
cd app
set OUT=C:\Users\user\Documents\sloptweak-focus-ab\out-live
set CONFIG=C:\Users\user\Documents\sloptweak-focus-ab\config.json
node dev/focus-live.mjs
```
Defaults: `MAX_DPH=0.75 IDLE_MINUTES=60 MAX_SESSION_MINUTES=180 HEARTBEAT_MINUTES=10
READY_TIMEOUT=30 HARD_CAP_MINUTES=75 KEEP=1`. `MOCK=1` runs it against the mock for $0.

## Pre-flight (the script refuses to continue if any fails)
- No `sloptweak*` instance on the Vast account; no `active_instance.json`.
- Credit at least **$3.00** (so the worst case above is covered).
- `VAST_API_KEY` present in your user environment (never printed); the debug build is
  current (I rebuild right before the run).
- `OUT` outside the repo; every picture in the config exists and has a size and face box.

## Steps
1. You say go (the numbers above, or changed ones).
2. The script checks the pre-flight, writes the run's `settings.json` (your original is
   saved beside it), starts vite and the app, picks Wulver Identity Edit and presses
   Start. The instance id is printed the moment it exists.
3. Wait for Ready. Log the host (GPU, VRAM, location, $/hr).
4. Per case: load the pictures, then two **blocks**, one per arm, in a random order
   (the face is painted once for the focus block). Within a block every image of the
   case is made in turn. Both arms of a pair use the same seed, prompt, picture(s) and
   shape.
5. Each image is saved as `pNN_x.png` or `pNN_y.png`; which of x and y is the focus arm
   is a coin flip per pair. The answer, seconds, sizes and the crop the app cut go to
   `key.json`. `review.html` shows the references with x and y side by side.
6. Hand off: clear the hard stop, keep the catalog server running, print the summary,
   exit. App, vite and the instance stay up.
7. I check the Vast list once more, report the instance id, $/hr, credit and when the
   backstops fire, and tell you the machine is yours.

**Blindness:** the script's output and `live.log` never say which arm an image is, and
no message mentions painting. The arms are only in `key.json` and in the app's own log
(`app.log` in `OUT`). I will not open either until the ratings are done; please don't
either.

## Matrix (SFW, adult characters; 14 images, about 25 min)

| Case | Mode | Inputs | Pairs (seeds) | Prompt (same in both arms) |
| --- | --- | --- | --- | --- |
| A | New, 1 sheet, tall | fox, focus = head | 3 (111111, 222222, 333333) | "Close-up portrait of the character, smiling, sitting at a sunny cafe table with a coffee, soft daylight." |
| B | New, 2 sheets, tall | fox + tiger, focus = both heads | 2 (444444, 555555) | "The two characters sit together at a sunny cafe table, laughing." |
| C | Edit | husky + tiger sheet, focus = tiger's head | 2 (666666, 777777) | "Replace the character's head and face with the character from the reference sheet. Keep the pose, the suit and the white background." |

Case C is the closest thing to your "swap a new face onto a base image" idea, and the
tiger sheet is the hardest input for the whole-sheet arm (five views, a small head), so
it should show the biggest difference if there is one.

**The enlargement question can't be separated in this matrix.** From the mock's crop log
the fox head crop is about 397x383 and the tiger's about 308x230, both under 0.25 MP, so
every focus image goes in enlarged to 0.5 MP (the tiger by about 2.7x). If the focus
images look soft, a follow-up pair on the held machine with the enlargement off (a
rebuild and an app restart, which Reconnect covers) is the way to tell; I'll do that
only if you say so.

## Judging
- **Blind, side by side** (`review.html`), references on the left, `x` and `y` on the
  right. Per pair, rate each of: face likeness to the sheet (1 to 5), outfit and body
  kept (1 to 5), repeats the sheet's multi-view layout (yes/no), follows the prompt
  (1 to 5). You rate; I rate separately; then we compare against `key.json`.
- There is no cheap objective identity metric in this repo (no face-embedding model),
  so this is a human judgement on 7 pairs. It can show a clear win or clear harm; it
  cannot show a small effect, and I'll say so in the write-up.
- **Adopt as is** if focus is rated better or equal on likeness in at least 5 of 7
  pairs with at least 3 clear wins, and nothing regresses in outfit or prompt
  following. **Rework** (try gray-out or crop-plus-gray-out, the other two options you
  were offered) if it wins on likeness but hurts the rest. **Drop or leave optional**
  if it's a wash or worse.

## Record
Per image: seconds, size, the crop the app cut and whether it was enlarged (all in
`key.json`), the ratings. Per run: instance id, offer, ready time, credit before and
after, when the machine was handed over. Written to `docs/findings.md` under a new
"Focus A/B" heading (images stay in `OUT`).

## Not covered
Identity across many seeds beyond 7 pairs, hands or full-body crops, very thin paint,
EXIF-rotated sheets, and the cold-start time of a never-seen host.
