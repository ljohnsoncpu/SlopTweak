# What SlopTweak costs, and how billing works

**SlopTweak itself is free.** You pay only Vast.ai, from credit you add to your
own Vast account. Nobody else sees your card or your bill, and the SlopTweak
authors never get any of the money.

## What you pay for

| What | Typical amount | When |
| --- | --- | --- |
| **GPU time** | about **$0.10 to $0.15 per hour** (the app shows the exact price before you start) | Every second from the moment the GPU is rented until it's shut down, including the setup minutes |
| **Model download** | **$0.00 to about $0.30 per session**, depending on the host | Once per session. Some hosts charge per GB downloaded; a 7 GB model on a pricier host costs about $0.27. The app picks hosts with the lowest *total* (hourly price plus download) and shows the download part separately. |
| Disk space | included in the hourly price shown | While the GPU exists |

**Example:** a one-hour session on a $0.11/hr GPU with a $0.02 download costs
about **$0.13**. $5 of credit is roughly 30 to 40 hours of use.

The price per hour comes from whichever host is cheapest right now, within your
limit (**Settings → Most you'll pay per hour**, default $0.50). Prices change
from minute to minute; the one shown when you press **Start** is what you'll pay
for that session.

## Where to see it

- **Before starting**, the home screen shows the cheapest GPU right now and the
  download cost.
- **While running**, the top bar (and the Invoke window's title) shows
  `$/hr · time · ≈ spent so far · credit left`. Credit is refreshed every few
  minutes.
- **Your Vast account** ([cloud.vast.ai/billing](https://cloud.vast.ai/billing/))
  has the exact charges. Vast sometimes posts small charges a little later.

## When billing stops

Billing stops when the GPU is **destroyed**. SlopTweak never "pauses" a GPU,
because a paused GPU still bills for its disk. It destroys the GPU when:

| Trigger | Default | Change it in |
| --- | --- | --- |
| You press **Stop** | right away (after the last images are copied, at most 1 minute) | — |
| You close SlopTweak while a GPU runs | after you confirm | — |
| Nothing is happening in Invoke | after **20 idle minutes** | Settings |
| Session length limit | after **4 hours** (warning 10 minutes before) | Settings |
| Your PC crashes, sleeps, or loses the internet | about **10 minutes** after SlopTweak last checked in | — |
| The GPU can't be set up | right away; SlopTweak tries up to 3 GPUs in total. If your PC is off by then, about 10 minutes after setup stopped | — |

The GPU shuts **itself** down in the idle, time-limit, lost-contact, and failed-setup
cases, so it works even when your PC is off. The next time you open SlopTweak it checks
for anything left running and offers to shut it down.

Your images are copied to your PC as you make them. If any couldn't be copied
before a shutdown, SlopTweak says so instead of reporting everything saved.

## Protecting your credit

- SlopTweak won't start if your credit is below **$1.00** (Settings), and warns
  when your credit covers less than an hour.
- **Vast's auto top-up.** Vast can refill your credit from your card
  automatically when it gets low. It's a setting on Vast's billing page, not in
  SlopTweak. If you want a hard spending limit, leave it **off** and add credit
  by hand.
- Vast's minimum deposit is **$5**.
- If something looks wrong, open [Vast's instances page](https://cloud.vast.ai/instances/):
  anything listed there is billing. Destroying it there is always safe.

## What Vast calls things

- **Instance**: the rented GPU machine.
- **Credit**: your prepaid balance. (Vast's "balance" field is something else;
  SlopTweak reads credit.)
- **Destroy**: delete the instance; billing stops. SlopTweak only ever destroys,
  it never "stops" an instance.
