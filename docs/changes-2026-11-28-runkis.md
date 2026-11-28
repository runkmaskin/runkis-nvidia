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
