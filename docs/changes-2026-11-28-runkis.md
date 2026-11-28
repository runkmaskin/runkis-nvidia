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

## Where the card's time goes (2026-11-28, in progress)

The user's city run at night (Series S, DLSS Quality to 1440p, 5,700-8,800 draws) showed 12.8-15 fps with the F3
panel's GPU BUSY at 62-73 ms. That figure is the frame's time less the stretches in which nothing of ours was
queued (`card_clock_line` in `crates/victory-gpu/src/host.rs`): a list waiting for the card, and the system's time to
start a submission and report it done, count as busy too. It does not say what the card worked on.

- **`VICTORY_CARD_PARTS=1`** (`crates/victory-gpu/src/host/parts.rs`, every build, off by default): a timestamp
  list stands in front of every list of every submission and behind the last, and each list's time on the card is
  summed by what it is: the title's lists, the preludes (uploads ahead of a list), our list of steps done on the
  card (`VICTORY_CARD_STEPS`), the compare passes after a submission's lists, and the list that sends what changed
  up to the copies in the card's memory and hashes those pieces again. The frame line gets "the card's time by part
  per frame", with the time from each submission's first stamp to its last. 4,096 stamps are made once and used in
  turn; a stamp is read when it is used again, some ten frames later.
- **Title memory left in the PC's memory**: a new line after "host copies in the card's memory" counts, since the
  start, the title buffers and windows that were to go to the card's memory but stayed in the PC's (the copies'
  limit `VICTORY_CARD_BUFFERS_MB`, the windows' `VICTORY_CARD_WINDOWS_MB`, or no room), with their bytes. Shaders
  read those over the bus; on this card that once made the heaviest compute passes take 20-40 ms each.

What the layer itself gives the card per frame, from the code and the user's log of that run (02:34, city):

| Ours on the card | Per frame | Cost, estimated from the code |
|---|---|---|
| Compare passes after the lists (`CARD_COMPARE`, a group per 64 KiB piece) | 430 passes, 37,170 pieces = 2.4 GB read and hashed; each of about 30 compare lists starts with a full UAV barrier | about 5-8 ms at the card's 504 GB/s |
| Sends up to card copies and the hashes after them (`card_before`) | 27-33 MB over the bus in about 680 runs and 230,000 scattered words; 180 rehash passes over 315 pieces | 1-2 ms |
| Steps done on the card (`VICTORY_CARD_STEPS=2`) | 11,346 fills of 5.8 MB: as many small buffer copies, a full UAV barrier first in each list | 1-5 ms (many tiny copies) |
| Indirect argument clamps in batches (`VICTORY_INDIRECT_BATCH=2`) | about 350 batch passes for 5,500 executions, a barrier after each | 1-5 ms (pipeline drains) |
| The light stand-ins (`VICTORY_AMBIENT*`) | a few small passes at 64x36 and 160x90 points | under 0.5 ms |
| Keeping the card awake (`VICTORY_CARD_AWAKE=1`) | one group of 64 threads, 2 ms at a time, on a compute queue of its own | little work; with hardware scheduling off it may cost the drawing queue turns (the old base measured +1 fps without it on an RTX 2070 SUPER) |
| DLSS to 1440p | one upscaling | about 1-1.5 ms |

The rest is the title's own lists.

### Measured (city at night, standing; last 90 s of a 240 s run; release build with `VICTORY_CARD_PARTS=1`)

| Series S, to 1440p | fps | 1 % low | the title's lists | compares | sends and hashes | all lists |
|---|---|---|---|---|---|---|
| DLSS Quality (two runs) | 15.1 / 13.8 | 7.0 / 8.4 | 44.1 / 55.3 ms | 6.7 / 6.4 | 2.7 / 2.1 | 54.5 / 64.7 |
| DLSS Performance | 14.6 | 2.8 | 39.9 | 6.7 | 2.4 | 49.8 |
| DLSS Ultra Performance | 15.7 | 7.7 | 43.6 | 7.2 | 2.0 | 53.6 |
| DLSS off (the title's upscaler, 720 lines) | 13.2 | 5.1 | 48.7 | 7.1 | 3.6 | 60.4 |

- The card's time does not follow the picture's size: Ultra Performance draws a ninth of Quality's pixels and is
  1-2 fps faster at most. Our own passes are about 10 ms of 55; the title's lists are the rest.
- The card is 95-98 % busy by `nvidia-smi` at its full clock (2,820 MHz) but draws 85-105 W of its 220 W: it waits
  for memory, not for its units.
- The new line says why: 600-820 of the title's windows, 2.5-3.5 GB, stayed in the PC's memory because windows on
  the card stop at 1,010 MB on this card (`default_window_card_mb`: 9 % of the budget, at most 1 GB). Every draw
  that reads vertices, indices or constants from them crosses the bus.
- A Series X run (DLSS Quality) hung the card after 106 s in the world (the log, the frames and `nvidia-smi`
  stopped together); not looked into further, and no Series X runs since.
- The first runs of a build at a new path compile pipelines for minutes (draws dropped, flat untextured surfaces):
  the driver keeps pipelines by program path. `victory warm --used` with the same program made 4,114 in 4 minutes.

### Windows on the card: measured, not kept

Two rounds interleaved, Series S DLSS Quality, city (the game's idle camera moves between shots, so draws vary):

| | fps (round 1 / 2) | 1 % low | the title's lists | frames over 100 ms | card memory in use | textures waiting |
|---|---|---|---|---|---|---|
| as it is (windows to 1,010 MB) | 17.1 / 18.4 | 8.1 / 10.5 | 40.1 / 37.6 ms | 10 / 2 | 7.7 GB | 2,300 |
| `VICTORY_CARD_WINDOWS_MB=5120` | 17.3 / 18.5 | 10.5 / 10.4 | 38.0 / 36.2 | 4 / 3 | 8.8 GB | 9,600 |
| and `VICTORY_CARD_BUFFERS_MB=4096` | 17.0 / 17.3 | 10.7 / 11.2 | 38.8 / 38.5 | 2 / 0 | 8.5 GB | 8,200 |

With room for 5 GB, 2.8 GB of windows went to the card (another 0.4 GB was turned down at four fifths of the card in
use) and the title's lists took 1.5-2 ms less: +0.15 fps, inside the noise, with four times the textures waiting for
room. The default stays. The bus is not where the title's 37-55 ms go; the card still draws only 93-96 W at 95 % busy,
so the time is in the title's own passes as translated (next: a captured frame, timed pass by pass).