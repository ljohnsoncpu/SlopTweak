# Beta test kit

PLAN §4 Phase 5: 1–2 non-technical people each complete a session **without
help**, while you watch and write down where they get stuck. This page is for
the person running the beta.

## Before the session

- [ ] A published release (installer + `latest.json`) exists; send the tester the
      [latest release](https://github.com/ljohnsoncpu/SlopTweak/releases/latest)
      link, or [the user guide](user-guide.md) if you want them to read it first.
- [ ] Decide who pays. The tester makes **their own** Vast account and adds
      $5 (Vast's minimum). A session costs roughly $0.10–$0.40
      ([costs](costs.md)). Tell them this up front.
- [ ] The tester needs: a Windows 10/11 PC, an email address, a payment card for
      Vast, and about 45 minutes.
- [ ] Mention the content rules: Vast's and CivitAI's terms apply, and the default
      model is flagged NSFW on CivitAI. Pick another model if that matters.
- [ ] If the installer isn't code-signed yet, warn them about "Windows protected
      your PC" → **More info** → **Run anyway**, or see whether they figure it
      out (that's a useful finding too).

## What to say

> "I'd like to see how easy this is without help. Please think out loud. I can't
> answer questions until the end. If you're truly stuck for 5 minutes, say
> 'I give up on this step' and I'll help, and we'll note it."

## Tasks (give them this list)

1. Install SlopTweak from this link.
2. Set it up with your own Vast.ai and CivitAI accounts.
3. Make an image of anything you like.
4. Do the inpainting tutorial: change one part of the sample picture.
5. Stop, and find your images on your PC.
6. Close SlopTweak and check on Vast's website that nothing is still running.

## Watch sheet (one per tester)

| Task | Done unaided? | Time | Where they hesitated / what they said |
| --- | --- | --- | --- |
| Install (incl. SmartScreen) | | | |
| Vast account + email verify | | | |
| Add credit | | | |
| Vast API key | | | |
| CivitAI account + key | | | |
| Start → Invoke opens | | | |
| First image | | | |
| Tutorial (inpaint) | | | |
| Stop → images found | | | |
| Nothing left running on Vast | | | |

Also note: did they understand the cost bar? Did anything scare them (the
CivitAI key warning, prices, SmartScreen)? What did they expect a button to do
that it didn't?

## After the session

- [ ] Ask them for **Settings → About and help → Copy diagnostics** and paste it
      into your notes (keys and tokens are removed).
- [ ] Check [cloud.vast.ai/instances](https://cloud.vast.ai/instances/) together:
      nothing should be listed.
- [ ] Note what the session cost them (Vast billing page).
- [ ] Turn each stumble into a GitHub issue labelled `beta`, quoting what they
      said. Fix, release, and re-test with the next tester.

**Pass (PLAN §4):** each beta user completes a session unassisted, meaning tasks
1–5 with no "I give up".
