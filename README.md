# runkis-nvidia

A tuned build of the VIctory compatibility layer for **NVIDIA RTX cards**. VIctory runs an Xbox Series title's own
executable on Windows: the title's x86-64 code runs natively, and the layer implements the console's APIs and
translates its graphics work and shaders to host D3D12. This fork is based on
[trustno1-coder/VIctory-NVIDIA](https://github.com/trustno1-coder/VIctory-NVIDIA). It includes renderer ports from
[avatarmanz/VIctory-7900XTX](https://github.com/avatarmanz/VIctory-7900XTX), plus fixes taken from
[louketa/foxhound](https://github.com/louketa/foxhound) and
[Yaekai/victory-scarlett-3070](https://github.com/Yaekai/victory-scarlett-3070).

> **No game files are included.** You need your own copy of the title (Xbox Series build v1.0.2.1).
> Not affiliated with or endorsed by the title's developer or publisher, nor by NVIDIA. GPL-3.0-or-later.

## Test PC

| | |
|---|---|
| CPU | AMD Ryzen 7 7800X3D (8 cores / 16 threads) |
| GPU | NVIDIA GeForce RTX 4070 SUPER 12 GB |
| RAM | 32 GB (31 GB usable) |
| OS | Windows 11, Balanced power plan, no system tweaks |
| Profile | Series S, 540p or 720p internal, the title's own upscaler, frames scaled to a 1440p window |

## Results (measured in game)

Measured on the PC above with an automated run: the title is started, scripted key presses take it through the
menus into the first gameplay scene (the prison), and frames are read from the layer's own per-frame log between
seconds 150 and 240. With the 60 fps cap the title reaches different scenes in that time, so runs vary by several fps; treat single runs as rough.

| Build | fps | 1 % low | frames over 100 ms |
|---|---|---|---|
| Defaults at commit 0458641 | 25.95 | 8.0 | 5 |
| + descriptor-table cache and early heap-record rounds | 27.14 | 14.0 | 0 |
| + the title's 30 fps lock lifted to 60 (two runs) | **26.9 / 35.5** | 14.0 / 9.6 | 0 / 5 |

- Menus and loading screens now run at the 60 fps cap. Gameplay runs at about **25–35 fps** depending on the scene.
  The game keeps its speed at 60 fps (it steps its world by elapsed time).
- **The GPU is not the limit**: it is busy 1–2 ms of a frame. The CPU work of the layer is.
- A first play session without warm-up hitched mostly on pipelines built during play (244 of 264 frames over
  100 ms). After **Warm up shaders** those are built ahead.

Measured on a second PC (Ryzen 5 5500X3D, RTX 2070-class, same save, 2 runs each against 16.98 fps) and now on by
default: fetches in four parts (`VICTORY_FETCH_PARTS=4`, +0.65 fps) and the pool settled once a second
(`VICTORY_POOL_SETTLE=1`, +0.45 fps).

## Where the time goes

At 540p a frame takes about 34 ms, with the GPU busy about 1–2 ms of it. The rest is the layer emulating the
console's **shared memory**: on the Xbox the CPU and GPU see one memory, on a PC they don't.

| Part (per frame) | Time | What it is |
|---|---|---|
| Preparing | ~23 ms | finding what the title changed (write watch, compares), refreshing host copies, translating command lists |
| Copying back | ~16 ms | bringing ~22 MB the GPU wrote back into title memory |
| Waiting for heap records | ~3–5 ms | mirroring descriptors the title wrote (was 10–13 ms) |

Two findings drive the current work:
- **Copy-back is mostly unneeded**: of the pages copied back, the title's own code read 0.1 % first; the layer's
  own threads read the rest. Landing them only on demand is in progress.
- **The title's own stores cost write faults**: a census of the title's write faults (`VICTORY_FAULT_BOOK`)
  found 144 a frame in the executable, led by a flag store that almost never changes anything.
  `VICTORY_WRITE_SITES` patches such stores in memory at load (never on disk) so they look first.

## How to run it

1. Build `target\diag\victory.exe`: `cargo build --profile diag -p victory --features diagnostics`
   (needs Rust and the Visual Studio Build Tools).
2. Keep the game **outside this folder** (for example `C:\Games\Xbox.v1.0.2.1`). The layer refuses game files,
   caches and saves inside the source tree.
3. Start **`LAUNCHER.bat`** (needs the .NET 8 SDK the first time), choose `game_scarlett_master.exe`, press
   **Warm up shaders** once, then **PLAY**. Or drag `game_scarlett_master.exe` onto `PLAY.bat`.
4. After playing somewhere new, warm up again: pipelines are recorded while you play and built ahead next time.

The launcher ([launcher/README.md](launcher/README.md)):
- **Start as**: presets, including one worked out for your card, CPU and RAM. It remembers your choice.
- **Warm up shaders** with progress in per cent and time left.
- **Settings**: render height (480–720p on Series S), **upscale to the window (sharp)** for screens above 1080p,
  picture fixes.
- **Controls**: every Xbox / PS5 button can be bound to a key or mouse button. The mouse turns the camera, left
  button fires, right aims.
- Live fps, frame times, 1 % lows, CPU, RAM, GPU and VRAM, the log with known problems explained, and a benchmark
  page for A/B runs.

Needs: Windows 10/11 x64, an NVIDIA RTX card, **32 GB of RAM** (the layer's process commits about 25 GB, part of
it card memory), an SSD with about 25 GB free for the driver's shader cache.

## Known issues

- **1080p internal crashes at start on the Series S profile**: the title runs out of its own memory pool and
  stops. 720p is the highest that runs. The Series X profile at 1080p runs at about 13 fps and fills 32 GB of RAM.
- **RAM use is high**: about 25 GB committed after a few minutes. Close other large programs while playing.
- **Bright haze** when facing the sun or bright windows: the title's ray-traced light is not computed on NVIDIA
  yet, so a stand-in light is used. In the launcher, tick "Leave out … heat haze" and lower Air. The structure
  builder that ray tracing needs now translates for 32-lane GPUs (52 of 52 programs) but has not been run in game.
- **Speed-set glitches**: occasional stretched bodies, dark shadow wedges on walls, and things through walls.
  The "Fewer glitches" preset removes them at about −29 % fps; targeted fixes (`VICTORY_CROSS_WAIT=2`,
  `VICTORY_TEXTURE_RECHECK=1`) are not yet measured. See [docs/speed-mode-bugs.md](docs/speed-mode-bugs.md).
- A phone screen can be black for a moment at the start of a cutscene; glass blur can show over a character
  standing in front of glass. Under investigation.
- Textures can appear late or briefly wrong while the title streams.

## More

- What changed on 2026-11-27, every switch with what was measured, and what still needs a game run:
  [docs/changes-2026-11-27.md](docs/changes-2026-11-27.md).
- Where the submitting thread's time goes: [docs/submit-thread.md](docs/submit-thread.md); copy-back:
  [docs/copy-back.md](docs/copy-back.md); heap records: [docs/heap-records.md](docs/heap-records.md).
- For AI coding agents working on this repository: [CLAUDE.md](CLAUDE.md).
- Earlier measurements on an RTX 2070 SUPER / Ryzen 5 5500X3D (17–19 fps at 540p) and the changes that got there
  are in the git history of this file; the upstream README with full background and credits is in
  [UPSTREAM-README.md](UPSTREAM-README.md).
