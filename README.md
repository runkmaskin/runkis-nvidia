# runkis-nvidia

A tuned build of the VIctory compatibility layer for **RTX 20-class NVIDIA cards and 6-core CPUs**. VIctory runs an
Xbox Series title's own executable on Windows. This fork is based on
[trustno1-coder/VIctory-NVIDIA](https://github.com/trustno1-coder/VIctory-NVIDIA). It includes renderer ports from
[avatarmanz/VIctory-7900XTX](https://github.com/avatarmanz/VIctory-7900XTX), plus fixes taken from
[louketa/foxhound](https://github.com/louketa/foxhound) and
[Yaekai/victory-scarlett-3070](https://github.com/Yaekai/victory-scarlett-3070). Every change here was measured
on the PC below before it was kept.

> **No game files are included.** You need your own copy of the title (Xbox Series build v1.0.2.1).
> Not affiliated with or endorsed by the title's developer or publisher, nor by NVIDIA. GPL-3.0-or-later.

## Test PC

| | |
|---|---|
| CPU | AMD Ryzen 5 5500X3D (6 cores / 12 threads) |
| GPU | NVIDIA GeForce RTX 2070 SUPER 8 GB (Turing) |
| RAM | 32 GB |
| OS | Windows 11, hardware-accelerated GPU scheduling on, High performance power plan |
| Profile | Series S, 960x540 internal (half scale), the title's own upscaler to 1080p |

## Results

**About 17–19 fps** in the opening scenes, and **up to about 26 fps** when little is on screen. 1 % lows are around
7 fps. After loading, no freeze is longer than about 0.5 s.

The game starts at roughly 4–9 fps on the build this one began from. The biggest steps:

| Change | Effect (measured) |
|---|---|
| VIctory-NVIDIA instead of the older build | 4–9 → ~9.6 fps, correct picture |
| GPU crash diagnostics off (`VICTORY_D3D12_DRED=0`) | 2–4x less GPU time on Turing |
| Memory compares on the card down to 64 KB (`VICTORY_CARD_BUFFERS_MIN_KB=64`) | about +10 % |
| Renderer ports 1, 2, 6 from the 7900 XTX fork | CPU cost per graphics call 9.4 → 6.8 µs |
| "Keep-awake" GPU job off (`VICTORY_CARD_AWAKE=0`) | 17.7 → 18.7 fps, half as many stutters (it hurts NVIDIA) |
| Pipeline wait 0.5 s instead of 10 s | a slow new shader shows a moment later instead of freezing the picture for seconds |
| Texture mip drop for textures 2048 px and larger | less pressure on 8 GB of VRAM; 1 % low 6 → 7.2 fps |
| 6 memory-compare threads instead of 3, woken only when there is work | small CPU saving |
| Big-compare thread split fix (from Yaekai) | a compare could wrap below zero and skip memory |
| Render-target reserve 1.5 GB instead of 3 GB (`VICTORY_CARD_ROOM_FOR_TARGETS_MB=1536`) | twice the textures on the card (1.2 → 2.3 GB), far less texture churn, worst frame 0.7–1.2 s → 0.2 s |
| Pool discard when free RAM is under 4 GB (from foxhound) | keeps a 32 GB PC from paging (one such PC ran at 4–5 fps with RAM full) |
| Pipeline recording (`VICTORY_PIPELINE_BOOK=1`) | lets `-Warm` build every pipeline ahead of play |

## Where the time goes

A frame takes about 55 ms on this PC, and the CPU is the limit (about 86 % busy on average).

- **About 40 ms of CPU** goes to translating the title's graphics work. The largest parts are mirroring the
  descriptor heap (about 11 ms), syncing draw tables (8–13 ms) and merging copied-back memory (6–9 ms).
- **About 20 ms** goes to copying GPU-written memory back. The Xbox shares one memory between CPU and GPU, and a PC
  has to emulate that.
- **The GPU is about 95 % busy** by the layer's own count, so a weaker GPU also costs frames.

Frames with many draws (6,000–7,000+) are the stutters. The first frame of the game world takes about 4 s while
thousands of buffer copies are made.

## Tried and not kept

| Tried | Result |
|---|---|
| `VICTORY_WC_NO_BACK` (no copy-back of write-combined memory) | +16 % on paper, but **black flashes** and about a quarter fewer draws |
| Upstream with the cool_fox renderer, no DLSS | 13.8 fps (slower) |
| foxhound as a whole (no half scale on this card) | 13.7 fps, but copy-back 12 ms instead of 22 |
| New upstream on the foxhound base + DLSS | broken hair and missing letters in text on this PC; very slow on first start |
| DoubleGhosty/cool_fox | crashed with these settings |
| `VICTORY_SPANS_QUICK` + `VICTORY_SYNC_FINE` | no gain; longer draw-table waits |
| `VICTORY_HEAP_SYNC_FIRST` | slower, worse 1 % lows |
| Texture mip drop from 1024 px | removes VRAM overflow completely, but visibly blurrier |
| Release build instead of diagnostics build | not faster |
| Port 5, 480p, removing barriers, skipping hair physics | 0–2 %, not worth it |

These figures come from an automated bench: a fixed save, scripted input, a 40 s warm-up, then 90 s measured, with
frame pacing read from the layer's own per-frame log. Four identical runs vary by about ±0.4 fps, so smaller
differences are noise.

## How to run it

1. Build it, or use a prebuilt `victory.exe` placed at `target\diag\victory.exe`:
   `cargo build --profile diag -p victory --features diagnostics`. This needs Rust and the Visual Studio Build Tools.
2. **Drag `game_scarlett_master.exe` onto `PLAY.bat`**, or copy your game folder next to it and name it `game`.
3. The first start is slow while shaders are translated and pipelines are built. Later starts are faster.

Or use the **launcher** (`LAUNCHER.bat`, see [launcher/README.md](launcher/README.md)): the same tuned settings,
saved profiles, a pre-flight check of the build, folders, card, RAM and drivers, live FPS / frame times / 1 % lows /
CPU / RAM / GPU / VRAM, the log with known problems explained, and the pipeline warm-up and shader translation as
buttons.

In a source checkout PLAY.bat now keeps caches and saves under `%LOCALAPPDATA%\victory`: the layer refuses folders
inside the source tree, so before this a checkout's PLAY.bat stopped at start.

Options for `scripts\play-nvidia.ps1` (`-PrintEnv` prints the resulting settings as JSON without starting anything;
the launcher uses it):
- `-Warm` builds every recorded pipeline into the driver cache before play. This needs about 23 GB free on C: for
  all of them; `-WarmLimit n` builds fewer.
- `-Precompile` translates every shader ahead of time.

Needs: Windows 10/11 x64, an NVIDIA RTX card, 32 GB of RAM (the game uses about 20 GB), an SSD.

**Getting 30 fps on screen (RTX 20):** real fps is CPU-bound at about 17–19, so use frame generation outside
the game: Lossless Scaling with LSFG 3, adaptive mode, a 30 fps target, flow scale 50 %, WGC capture, max frame
latency 1. Input still feels like the real rate. For a sharper picture at no fps cost, turn on NVIDIA Image
Sharpening for `victory.exe` (0.5 sharpen, 0.17 film grain).

**Bodies/textures flying through walls:** `-Quality` (`QUALITY=1` in the start script) fixes them by turning off
`RELAY=2` and `CARD_STEPS=2`, at about −29 % fps.

**Tips:** close the browser and video players while playing (a YouTube tab costs about 0.8 fps here). Keep the
NVIDIA shader cache size at "Unlimited" or large. Frame generation (DLSS FG, Smooth Motion) needs an RTX 40/50; on
RTX 20, an external tool such as Lossless Scaling can add generated frames.

## Known issues

- Ray-traced lighting is not computed (a stand-in light is used), so some rooms look too bright or too dark.
- Textures can still appear late or briefly wrong while the title streams.
- The first frame of the world freezes for about 4 s while it loads.

What changed on 2026-11-27 (audit fixes, memory work, the launcher) and what still needs a game run:
[docs/changes-2026-11-27.md](docs/changes-2026-11-27.md).

The upstream README with full background and credits is in [UPSTREAM-README.md](UPSTREAM-README.md).
