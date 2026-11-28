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
## Review of the port (2026-11-28, later)

- **`VICTORY_FENCE_SLEEP`, read from the title's code.** The patched call is in a generic fence wait
  (+0x4fb0ad0: fence object, target, a callback pair, a flag), used by many threads, not only at the end of a
  frame (8,192 waits in the first 40 s of a run). On the path patched, `r13` is set to `0x80000000000000`
  (bit 55) right before the loop, so the stub's compare is the title's; it now takes `r13` itself anyway. After
  `Sleep(0)` the loop calls +0x10a0c0 with a local object: a GPU-hang watchdog (only when a title flag is on)
  that, after ten turns without progress and about 10 s on the clock, calls the title's device-lost handler. Its
  clock is wall time, so blocking does not change when it fires. The stub returns to the title's own compare,
  so the loop leaves at the same value as before; what changes is when the waiting thread wakes and how many
  turns it makes. `VICTORY_FENCE_SLEEP_MOST_MS` sets the bound, and the log says how many waits gave up at it.
- **The mouse stick** (`VICTORY_MOUSE`): it was dropped with `VICTORY_ONE_PAD=0`, read one 8 ms poll at a time
  (slow moves read as 0 at most of the title's frames), grabbed the cursor into the console window, and read a
  jump on coming back to the window. All fixed (commit "Review fixes of the port commits").
- **`VICTORY_TITLE_PRIORITY_CAP`** below 0 also lowered normal threads; only raised ones are capped now.
- **The launcher's profiles.** "Stad (768p, 2K)" and "Min PC (768p, 2K)" (schema 1) become `--series-s --dlss
  quality` (a 720-line scene of the 1080p output), `VICTORY_FULLSCREEN=1` (they were borderless), no frame rate
  cap, frame generation ready but off, and `VICTORY_MOUSE=1`; Stad keeps its own data folder. "2K" was the old
  base's present scale to the monitor; on this base the 1080p output fills the screen through Windows. With
  `VICTORY_MOUSE=1` the native mouse (`VICTORY_MOUSE_CAPTURE`) is not fed, by design; F1 lets the cursor go and
  the stick reads nothing while the menu is open. Upstream's own path gives the title no mouse unless
  `VICTORY_MOUSE_CAPTURE=1`, which is why the mouse did not steer there.
- **The view heap cap.** An RTX 4070 SUPER says `MaxViewDescriptorHeapSize` 1,000,000 and makes no larger
  shader-visible heap: the cap is the card's and cannot be raised there. The title's heap has 1,245,184 slots, of
  which the host keeps 737,855 beside its 2^18 dynamic slots. In the city the title rewrites all of the rest
  (up to slot 1,245,182, about 3 million slots a second): its own ring of per-draw tables, which the layer
  makes in its dynamic slots; no run warned of a draw asking for one of those slots directly, so nothing shows
  that views are lost there. The heap sync counts those slots (info line). `VICTORY_DYNAMIC_SLOTS` now also
  applies on such a card (2^17: 868,927 title slots); in its one city run (V2) the card's time a frame was
  66-71 ms against 36-40 ms without it, the copies 66-74 MB against about 45: keep it off.

### Game runs of the review (city save copy, Series X 1440p DLSS Quality, scratch `VICTORY_HOME`)

Captures of the game window (PrintWindow) at the seconds named; every run reached the world (DLSS running) at
47-68 s.

| Run | Build | Switch | 90 s | 120 s | 150 s | 180 s | 195 s |
|---|---|---|---|---|---|---|---|
| A | `b98bf65` | none | | the room | the room | the room | |
| B | `b98bf65` | `VICTORY_FENCE_SLEEP=1` | | **white** | **white** | (run closed at 04:33) | |
| A2 | this branch | none | the room | the room | the room | the room | the room |
| B2 | this branch | `VICTORY_FENCE_SLEEP=1` | the room | the room | the room | the room | the room |
| C2 | this branch | `VICTORY_FENCE_SLEEP=1`, `_MOST_MS=1` | the room | the room | the room | the room | the room |
| V2 | this branch | `VICTORY_DYNAMIC_SLOTS=131072` | **white** | (run closed at 105 s) | | | |

The white picture came in runs with different switches (B, V2) and, by the coordinator's report, in another
agent's run with neither (`VICTORY_CARD_PARTS=1`); B2 and C2, with the fence switch, were not white at any
capture. It is not the fence switch's: the switch keeps the title's exit condition, and the same switch ran
clean twice. What makes a picture white for a while in the world is still open. B's log: 15 fps and 8 ms of
the card a frame while white, against 10 fps and 44 ms in A: the card did less work a frame while it was
white.
- **Tests and lints.** The two `indirect::tests` failures on upstream were tests written for
  `VICTORY_INDIRECT_REST` on, which upstream turned off by default; both now test the default.
  `cargo clippy --workspace --all-targets -- -D warnings` is clean, with and without `--all-features`.
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

### The title's passes ranked (timing run, city at night, Series S DLSS Quality)

`VICTORY_GPU_TIMING=1` (diagnostics build) puts a timestamp before every command the layer records; it now does so
without device-removed data too (`HostList::note`), whose breadcrumbs cost the card 2-4x on NVIDIA. Submissions are
waited for one by one in such a run, so the frame is slower, but each command's time on the card is its own. A
captured frame (`VICTORY_RECORD`) could not be used: its replay lost the device (0x887A0006) on this card, with and
without `VICTORY_RECORD_ALWAYS=1`.

Of 32.6 ms of the title's commands a frame (mean of 20 lines): indirect executions 21.7, dispatches 6.6, mesh 1.5,
draws 0.8.

| ms a frame | program (shader hash) | console wave | barriers / LDS ops / wave ops in the translation |
|---|---|---|---|
| 8.2 | `water_shading:CS_TransmissionVolumetrics_Tiled_LocalLights` (6f225b641c5b0708) | 64 | 163 / 241 / 110 |
| 2.5 | `vfx_proj_decal_vbuffer:CS_ProjDecalReduceClassification_Scalarized` (1f1fbd86759b3da2) | 64 | 148 / 217 / 156 |
| 1.0 | alias copies (82 a frame) | | |
| 0.7 | `sr_tracing:CS_RayMiss` (06bb6e728c7c99bf) | 32 | |
| 0.6 | a vertex+pixel indirect draw (e732a8b4bea4c0dd) | | |
| 0.5 | `cascadeshadows_rendering:CS_ShadowRenderTiledCascadesAlphaCB` (922e1d0ee1e19a85) | 64 | 91 / 133 / 76 |
| 0.5 | `indirect_diffuse_lighting:CS_ScreenProbes_TemporalResolve_ReflectiveCaustics` (34fb51b8a0312e82) | 32 | |
| 0.5 | `vfx_proj_decal_vbuffer:CS_ProjDecalResolve_Vectorized` (c4183d3cbee28e8e) | 64 | |
| 0.4 | `scattering_transmittance_system:CS_Gather_Classified_16` (84eafa50504bc9ef) | 32 | |

The two at the top are console 64-lane programs that a 32-lane NVIDIA wave runs as two host waves meeting in
shared memory: each exchange between the halves is a store, a group barrier, two loads and a second barrier
(`exchange_halves` in `victory-shader/src/emit/lower.rs`), and the water program runs in the wave-wide form (8
branches taken by the whole console wave, two barriers each). Nothing else costs more than 0.7 ms.

`VICTORY_WATER_LIGHT=0` (the F1 menu's WATER LIGHT, upstream's switch) leaves the water's two tiled light programs
out. Interleaved, same spot:

| | fps | 1 % low | median frame | frames over 100 ms | the title's lists |
|---|---|---|---|---|---|
| water light on (default) | 17.4 / 16.8 / 16.6 | 8.6 / 9.0 / 10.8 | 59 / 62 / 63 ms | 29 / 10 / 3 | 41.2 / 38.9 / 39.8 ms |
| `VICTORY_WATER_LIGHT=0` | 19.4 / 19.9 / 20.3 | 11.2 / 10.7 / 11.6 | 48 / 47 / 46 ms | 1 / 1 / 1 | 32.8 / 32.9 / 32.0 ms |

+17 % (three interleaved pairs) and almost no frames over 100 ms. What it costs the picture: the water has no light of its own depth (darker,
bluer; at night on the balcony barely visible). Left as the player's choice (F1, WATER LIGHT), not the default.

Candidate translations, not done yet: an exchange between the two host waves needs its second barrier only so that
the slots are not written again before both halves have read them; alternating two sets of slots per exchange (both
halves exchange equally often) would make it one barrier, about half of these programs' barriers.