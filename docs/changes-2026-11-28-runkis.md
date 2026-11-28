# runkis-nvidia on the upstream base (2026-11-28)

On 2026-11-28 runkis-nvidia moved onto
[trustno1-coder/VIctory-NVIDIA](https://github.com/trustno1-coder/VIctory-NVIDIA) `a4d05e7` (GPL-3.0-or-later)
as its base. The old base (the u0221 lineage, kept as branch `old-u0221`) showed picture faults on two PCs
(see-through characters, glass over hair, dither dots, white-outs, rainbow skin) that upstream run as it is did
not. Only this fork's own changes that upstream lacks come over, each behind a switch that is off by default, so a
plain start is upstream's. Upstream's rendering is the reference: the old base's picture experiments (early depth,
plane back, the stand-in light values, its own TAAU and DLSS path) are not carried over.

## What upstream already had

| Our change (old base) | On this base |
|---|---|
| 60 fps cap, `VICTORY_FRAME_INTERVAL_US` | `victory play --fps` (default 1000, no cap); `apps/victory/src/play.rs` |
| Title timer step (`VICTORY_TITLE_STEP`) | upstream's own: `crates/victory-loader/src/frame_rate.rs` patches the step with the rate |
| Fast warm-up of the pipelines played (f058aed) | `victory warm --used` (early pipelines first, done list per program and card, a process per batch) and `victory preload`, which also fills the pipeline library. Our naming by host description saved about 11 % of the warm-up time offline; not ported |
| RAM: `VICTORY_RAM_TRIM` (spare scratch chunks and staging blocks) | `VICTORY_ALLOCATOR_TRIM` (allocators and spare scratch chunks, skornuta's memory stack), `VICTORY_SLOT_EVICT`, `VICTORY_BUDGET_PACK`; not ported |
| RAM: `VICTORY_TEXTURE_UNMAPPED_GO` | measured fps-neutral on the old base (a commit-charge saving only); upstream counts these copies in its memory line and evicts under pressure with `VICTORY_SLOT_EVICT`; not ported |
| Display settings (`VICTORY_WINDOW`, `_SIZE`, `_MONITOR`) | `VICTORY_FULLSCREEN` (borderless, covering the monitor) and the F1 menu |

## Ported

- **`VICTORY_FENCE_SLEEP=1`** (`crates/victory-loader/src/fence_wait.rs`, from cae3ec2): the title's end-of-frame
  fence wait calls a stub in place of its `Sleep(0)` helper (RVA 0x4fb0c22, the loop's bytes checked first) and
  blocks in `WaitOnAddress` until the title's own condition holds. Upstream's `fence::set` already wakes the
  fence's address, so only the loader's part came over. On the old base one run measured +0.9 fps and +1.8 on the
  1 % low. **On this base its one game run (Series X, city save) showed a white picture at 200 s while the run
  without it showed the room**; one pair is not a bisect, so it stays off and is not offered by the launcher's
  presets until a capture A/B clears it.
- **`VICTORY_TITLE_POOL=<factor or MiB>`** (`crates/victory-loader/src/title_pool.rs`, from 558f936 and d63e24a):
  the title's two Series S start-up pools made larger. Upstream's Series S with DLAA (a 1080-line scene) stopped
  at the boot screens in its game run (0x80000003 at +0x2219650, 31 s); the run with `VICTORY_TITLE_POOL=2` was
  stopped by the user before it ran.
- **`VICTORY_TITLE_PRIORITY_CAP=<-2..2>`** (`crates/victory-hle/src/modules/kernel.rs`, from cae3ec2): the title's
  "above normal" and "highest" threads (its job pools run 13 workers at "highest") at that priority at most, so the
  layer and the desktop keep processors. Not measured on this base.
- **The mouse and the stand-in pad's keys** (`crates/victory-hle/src/modules/gameinput.rs`, from e428351 and
  0458641). Upstream's mouse did not turn the camera in the user's test. Its path: `VICTORY_MOUSE_CAPTURE` is off
  by default, so the title is given no mouse at all; on, the deltas are `GetCursorPos` differences (Windows'
  pointer acceleration included, not raw input) inside a clip rectangle, and without `VICTORY_MOUSE_WARP=1` they
  stop at the window's edge. Whether this build turns its camera with native mouse axes at all is not shown (the
  old base traced the camera to the right stick's action values). Ported, each off unless asked for:
  `VICTORY_MOUSE=1` (the mouse as the stand-in pad's right stick past the 0.18 dead zone, buttons to the
  triggers; nothing while F1 is open), `VICTORY_MOUSE_SENSE` and `VICTORY_MOUSE_INVERT_Y=1` (for both mouse
  paths; scaled native deltas keep their fractions), and `VICTORY_KEYMAP` (unset: upstream's stand-in pad
  exactly, checked key by key). The launcher turns `VICTORY_MOUSE` on by default. Not yet run in the game.
- **`VICTORY_FRAME_LOG=<file>`** (`crates/victory-gpu/src/host.rs`, from 6fe8c3c): one line per presented frame
  (seconds, ms since the last, draws), for the 1 % low and the launcher's tiles and benchmark.
- **`victory play --print-env`** (`apps/victory/src/play.rs`): the start's settings and its `run` as one line of
  JSON; nothing is asked, fetched, written or started. The launcher builds its runs from it.
- **The Runkis launcher** (`launcher/`, `LAUNCHER.bat`): starts `victory play` with the profile's options
  (`--series-s`, `--res`, `--dlss`, `--framegen`, `--fg-type`, `--fps`) and its switches as `--set`, so the
  warm-up and F1 restarts keep them. Old profiles are read and become play options (see `launcher/README.md`).

## Game runs (7800X3D, RTX 4070 SUPER 12 GB, 2560x1440, city save, hardware GPU scheduling off)

Seconds 150-240 of each run, the first run of the new program discarded (10.2 fps while the driver compiled).

| Run | Profile | fps (frame log) | 1 % low | frames over 100 ms |
|---|---|---|---|---|
| A1: this branch, no switch | Series X 1440p DLSS Quality | 11.67 | 5.3 | 353 |
| B1: `VICTORY_FENCE_SLEEP=1` | Series X 1440p DLSS Quality | 13.28 | 4.6 | 173 |
| U1: upstream `a4d05e7` | Series S DLSS Quality | about 15.7 (its own fps lines, the last six, 5,200 draws; it reached the world late) | - | - |

A1 and B1 are one pair, in a scene that changed during the window (6,200 draws a frame, then 10,000 to 13,000 at
different seconds in the two runs), and still compiling pipelines: not a measurement of the switch. The second
round was stopped at the user's request.

## Dropped draws and pipelines in the city (2026-11-28)

The play log's "dropped ~300 without a host pipeline" and "~900 indirect" lines, classified in two diagnostics
runs of the city save (Series S, 1440p, RTX 4070 SUPER, this branch plus the fix below): a first start after a
translator change, then a second start. Per frame, in the 5 s lines:

| Reason | First start | Second start |
|---|---|---|
| A stage of the ray-tracing structure builder, which is off (`VICTORY_BVH_BUILDER` unset) | 300 (180-380) | 296 |
| `CS_RaytracingResolve`, left out without the builder (the "indirect" ones) | 860-950 | 900 |
| Named in `VICTORY_LEAVE_OUT` (decal heap defragmentation, heat haze) | 2 | 2 |
| Being made (the pipeline still compiling) | up to 2,880 for the first ~15 s of the world, gone ~80 s after it | **none** |
| Indirect mesh / dispatch, "prepare failed" | 37 + 14, in one 5 s window | none |

So the steady ~300 of the frame line are the builder's stages and nothing else: by design, not pipelines that
fail. The frame line's "being made" count is the pipeline queue (early starts included), not draws waiting: it
read 200-400 while no draw was dropped for it.

- **Pop-in while pipelines compile:** only on the first start after the translator or the driver changes
  (every shader is translated again): up to 2,880 draws and dispatches a frame dropped for about 15 s, then
  tens to hundreds for a minute. The second start had none in 5 minutes. `VICTORY_SHOW_NEW_LATER` is about
  texture slots and does not help here; the pipeline library the first start saves does. `victory preload`
  (shaders, then `warm`, then the library) does the same before play and should cover the city; it was not
  run here.
- **Driver refusals:** the six compute pipelines the old base's driver refused (`E_INVALIDARG`:
  2cc4c197ee29e291, 02384a60fda6f2d0, 6b82bc7d22016cb5, 5dc3b25986622949, 6f225b641c5b0708, 6871d6e4b3ac9a10)
  are not refused on this base: four are made, 02384a60fda6f2d0 is left out on purpose, and the other two did
  not translate ("no wave32 lowering for this wave64 program"). No pipeline was refused in any run on it.
- **Fixed: `CS_TransmissionVolumetrics` (6871d6e4b3ac9a10)**, the untiled volumetric light, groups of 8x8 like
  its tiled siblings: its digest joins the list that chooses the wave-wide form (`emit::emit`, 17 branches).
  It translates, validates and is made, and both runs had no device loss. The title asked for it in the city
  (the second start made it at once, as it does the pipelines earlier runs used), but it is not one of the frame line's drops, so its effect on the picture
  is not measured. Test: `an_lds_exchange_in_a_lane_branch_of_an_8x8_group_runs_wave_wide` (GPU-run). The
  change is in the translator, so every shader is translated again once.
- **Not fixed: `CS_CollectTriangles_Meshlets` (6b82bc7d22016cb5)**, groups of 1024: its own LDS is the
  host's whole 32 KiB, with no room for the exchange. It was not among the drops in either run.
- **Never made by design:** device methods 0x1c8 and 0x218 (not implemented, `device.rs`), at least 16,384 and
  2,048 calls a run. The title goes without those pipelines and draws nothing with them.
- **Translator warnings in the logs:** only the two "does not translate" lines above.

## The submitting thread and the card: what still serializes them (2026-11-28)

What upstream already overlaps (the play settings, `apps/victory/src/play.rs`):

- `VICTORY_RELAY=2` (with `VICTORY_LATE_WAIT` at stage 3): submissions are sent without a wait, a thread of its own
  (`victory-relay`) lands them when the card has run them, and a submission that reads title memory a flight's
  shaders may write waits only for the flights that declared that memory (`relay_wait`, `blocked_by`), not for
  everything in the air. The present is a flight too.
- `VICTORY_CARD_STEPS=2`: those reads (11,000 to 15,000 fills of constant buffers a frame in the city, out of the
  title's ring in its 174 MB heap, which the skinning passes declare written as a whole) are done on the card in
  front of the submission's lists, behind the flights, instead of waiting for them.
- `VICTORY_UPLOAD_AHEAD=1`, `VICTORY_JOIN_PARTS=8` (up to eight of the title's submissions prepared as one),
  `VICTORY_RELAY_COPY=1` (the relay's fetches on the copy queue).

So the old base's ideas are mostly there in another form. `VICTORY_SUBMIT_PIPELINE` of the old fork was never
code (`docs/per-draw-plan.md` there names it as stage 6, "only after stage 5"); `VICTORY_FETCH_IN_FLIGHT` cut the
old base's card-done-to-told from 29.4 to 2.1 ms with no fps change. Here the relay waits 4 ms a frame for its
fetches, and the title's own submissions are rarely held up by a landing (the card stood 0.8 ms a frame "while the
title had not submitted"), so it was not ported.

What still serializes them, from the logs of six city runs on this base (Series S 1440p DLSS Quality, RTX 4070
SUPER, the frame lines' "settles" and "card's clock"):

| Wait of the submitting thread | Per frame | What the card does meanwhile |
|---|---|---|
| "title memory read": a submission the card's steps refuse waits until the flights that declared what it reads have landed | 0.1 to 0.25 submissions, 4 to 14 ms | busy with the flights ahead, then **idle** during their landing and the preparing behind it |
| "lists in the air": 256 host lists in the air (`VICTORY_LATE_WAIT_LISTS`) | 3 to 7 ms | busy: a frame's worth is queued |
| title memory changed under shader writes, a memory value, a fence | 0 | - |

The refusal is always the same one: "a copy at its place in a list writes what it reads, or reads or writes what
it writes" (`card_steps::BEHIND_PLACED`). The card counts itself 86 to 93 % busy, idle "behind us" 4 to 8 ms a
frame. So the frame is close to the card's own time (title lists 39 to 41 ms, ours about 10 more), and what the
overlap can still win is those 4 to 8 ms.

- **`VICTORY_SUBMIT_PIPELINE=1`** (`crates/victory-gpu/src/host/submit_pipe.rs`, off by default; needs the relay's
  second stage). A submission of joined parts that would wait for landings is cut instead: the front parts that
  read nothing in the air go to the card at once, as a submission of their own, and only the rest waits (and may be
  cut again). The cut is where one of the title's submissions began, so each side is what the line sends without
  joining; the rest is checked joinable behind the front again, and a front with a copy at its place is not cut.
  So every CPU step reads the same bytes as with `VICTORY_JOIN_PARTS=1`, and as the joined submission does. Tests:
  `host::submit_pipe::tests` (the choice of the cut; a fixed-seed model of title memory, flights and parts, 4,000
  cases, where the bytes every part reads are the same cut, joined and unjoined). The frame line gets
  "submissions cut ahead of their wait for landings per frame".
- Upstream's own switches for the refusal itself, off there: `VICTORY_PLACED_READS=1` (fills behind such a copy read
  where the copy reads) and `VICTORY_PLACED_ORDER=1` (the submission is cut behind the copy's list). To be measured
  beside it.

**Not measured in game yet.** The A/B (off, `VICTORY_SUBMIT_PIPELINE=1`, `VICTORY_PLACED_READS=1` with
`VICTORY_PLACED_ORDER=1`, two interleaved rounds of 240 s after a discarded warm-up run, Series S 1440p DLSS Quality,
the city save) was queued when the game queue was paused by the user, and was withdrawn before it ran. What to
look at: fps and 1 % low from the frame log; the frame line's "title memory read" wait and "the card idle behind
us"; "submissions cut ahead of their wait" (how often a cut happens at all: when the first part is the one that
reads what is in the air, there is nothing to cut). Until then the switch stays off and is not in the launcher's
presets.
