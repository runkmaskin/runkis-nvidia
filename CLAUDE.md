# Guide for AI coding agents (Claude Code and others)

This file is read automatically by Claude Code. It tells an agent how this repository works, how to build, test,
run and measure it, and the rules that keep the work honest. Humans: README.md is for you.

## What this is

VIctory runs an Xbox Series title's own x86-64 executable (`game_scarlett_master.exe`, v1.0.2.1) natively on
Windows. It implements the console's APIs and translates D3D12.X and RDNA2 shaders to host D3D12/DXIL. This fork is
tuned for NVIDIA RTX cards. The fps limit is CPU work in the layer (about 48 ms of a 55 ms frame on one thread), not
the GPU. Read `docs/findings.md` and `docs/submit-thread.md` before any performance work.

| Path | What |
|---|---|
| `crates/victory-core` | Config, the asset-policy guard, logging |
| `crates/victory-loader` | Maps, binds and starts the title; crash reports; load-time patch sites |
| `crates/victory-hle` | Console API implementations (memory, DirectStorage, task queues, input…) |
| `crates/victory-gpu` | D3D12.X to host D3D12: lists, descriptors, host copies of title memory, copy-back, pipelines |
| `crates/victory-shader` | RDNA2 to DXIL translator (`gfx10` decode, `ir`, `structure`, `emit/lower.rs`) |
| `crates/victory-pipeline` | Content-addressed cache |
| `apps/victory` | CLI: `run`, `config`, `shaders`, `warm`, `translate`, `census` |
| `launcher/` | C#/WPF launcher (.NET 8); `LAUNCHER.bat` builds and starts it |
| `scripts/play-nvidia.ps1` | The tuned settings; the single source for PLAY.bat and the launcher (`-PrintEnv`) |

## Rule #1: nothing from the game in the repository

No game files and nothing derived from them, in any form: shaders, dumps, screenshots, logs with game data,
caches. See `docs/asset-policy.md`. `cargo xtask policy` checks what git tracks, and the hook in `.githooks/`
runs it. The layer refuses cache and data folders inside the source tree. Keep the game outside the repo.

## Setup (Windows 10/11 x64)

- Rust: `rust-toolchain.toml` pins 1.98.1. Install rustup; the toolchain installs itself.
- Visual Studio Build Tools with C++ (MSVC). The .NET 8 SDK for the launcher.
- A D3D12 GPU for the GPU tests.
- Build the binary the launcher and PLAY.bat use:
  `cargo build --profile diag -p victory --features diagnostics` (output: `target\diag\victory.exe`)

## Test before every commit

| Command | Time | Notes |
|---|---|---|
| `cargo test --workspace` | ~5 min | The shader tests run translated code on the GPU |
| `cargo test -p <crate>` | | While iterating |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | | What CI runs; also with `--all-features` |
| `cargo xtask policy` | | Asset policy |
| `dotnet test launcher\RunkisLauncher.slnx` | | Launcher tests; one starts the real `victory.exe` |

Pitfalls:
- `victory-gpu`'s `watch::tests` fail now and then, a different test each time. This happened on the base
  commit too (about 1 run in 4); rerun before chasing it.
- Windows PowerShell 5.1 reads UTF-8 files without a BOM as ANSI. Read and write text with
  `[IO.File]::ReadAllText(path, [Text.Encoding]::UTF8)`, not `Get-Content`/`Set-Content`, or "…" and "—" are
  mangled.
- cargo may not rebuild after a file is restored with an old timestamp. Touch the file when results look stale.
- Any change under `crates/victory-shader/src` changes the translator's hash. Every user's shader cache is then
  rebuilt on the next start: a slow first start, which is expected.
- With Windows PowerShell 5.1, don't name helper functions after built-in aliases (`R`, `rd`, `sc`…), and pass
  `InvariantCulture` when formatting numbers on non-English systems.

## How changes are made here

1. **Prove it first.** A bug fix gets a test that fails before the fix. A translator fix gets a GPU-run test in the
   style of `crates/victory-shader/src/emit/valu_tests.rs`.
2. **Speedups keep results identical.** Keep the old algorithm in the test as the reference, run fixed-seed
   inputs, and add a `cargo test --release` micro-benchmark that prints before and after.
3. **New behaviour goes behind a switch that is off by default.** A `VICTORY_<NAME>` env var, read once
   (`OnceLock`). Off must be byte-for-byte the old behaviour. Defaults change only after a measured game run.
4. **Don't trade drawing for correctness blindly.** Making the translator refuse a shader drops all of its
   pipeline's draws, which is often worse than a small error. Two known errors are kept on purpose; see
   `docs/changes-2026-11-27.md`.
5. **Don't claim in-game gains without a game run.** Label figures "offline" or "measured in game".
6. **Comments:** plain English, short declarative sentences, saying *why*, as in the existing code.
7. **Docs:** add what you changed to `docs/changes-*.md`. Commits end with a `Co-Authored-By:` line naming the model.

## Running and measuring

- **Launcher:** start `LAUNCHER.bat`, choose the game's exe, then **Warm up shaders** once (it translates every
  shader and builds the recorded pipelines), then **PLAY**.
- **First start:** the first start after a new build is slow for minutes. Never compare fps on a first start.
- **Caches:** shaders and pipelines are cached in `%LOCALAPPDATA%\victory\cache`; saves and `crashes.log` are in
  `%LOCALAPPDATA%\victory\data\<title id>\`.
- **Launcher logs:** `%LOCALAPPDATA%\RunkisLauncher\logs\<time>-play.log` (stderr and stdout of the run), plus
  `*.frames.txt`, one line per presented frame: seconds, ms since the last frame, draws; then ms in the title's
  present calls, ms in frame waits, ms sending the back-buffer copy, ms in DXGI Present, ms all threads waited for
  pipelines, and compiles (see `docs/frame-pacing.md` for how to read hitch lines).
- **Key log lines,** every 5 s:
  - `{fps} fps, {ms} ms/frame ({cpu} preparing: copies … textures …; {gpu} on the GPU; {back} copying back …)`
  - `memory by subsystem: private=… list_allocators=… windows=… gpu_used=…` (MB, each with `_peak`)
- **Benchmark page:** interleaved A/B runs (warm-up 40 s, measured 90 s, repeats), with optional scripted keys
  (`VICTORY_KEYS`, format in `launcher/README.md`). Differences under the noise floor are marked as noise; runs on
  the reference PC vary by about ±0.4 fps.

Measured in game on 8d8db49 (5500X3D, 2 runs each, all-off 16.98 fps) and now on in `play-nvidia.ps1`:
`VICTORY_FETCH_PARTS=4` (+0.65), `VICTORY_POOL_SETTLE=1` (+0.45). Measured and left off: `VICTORY_LAND_SEGMENTS=1`
(+0.34, noisy), `VICTORY_RETIRE_APART=1` (+0.21, worse 1 % low), `VICTORY_RELAY_COPY` stays as the script sets it,
`VICTORY_PORT_DESCRIPTORS_APART=1` (0), `VICTORY_POOL_PRIORITY=1` and `VICTORY_HEAP_SYNC_GPU=1` (worse 1 % lows).
Not yet measured: `VICTORY_LIST_CLASSES=1`, `VICTORY_SHARE_PIPELINES=1`, `VICTORY_HEAP_ROUNDS_EARLY=1`, `VICTORY_TABLE_CACHE=1`, `VICTORY_FENCE_SLEEP=1` (the title's frame fence wait blocks instead of spinning; one run +0.9 fps), `VICTORY_TITLE_PRIORITY_CAP=0`, `VICTORY_TITLE_WORKERS`, `VICTORY_TITLE_STEP=1` (docs/changes-2026-11-27.md), `VICTORY_WRITE_SITES=<file>` (a list of `game_scarlett_master.exe <offset>` lines; the fault book census found `or byte` flag stores at the top). Measurement-only switches for the next profile: `VICTORY_DRAW_PARTS=1`, `VICTORY_BACK_CENSUS=1`, `VICTORY_CROSS_WAIT=count`; more A/B candidates (`VICTORY_RETIRE_APART`, `VICTORY_LAND_SEGMENTS`, `VICTORY_AHEAD_GAP_KIB`, `VICTORY_POOL_PRIORITY`, `VICTORY_SET_CACHE`, `VICTORY_CALL_TIMES=0`) are in `docs/changes-2026-11-27.md`, `docs/heap-records.md` and `docs/copy-back.md`.

## Open work, in priority order

1. **Measure.** Benchmark the presets and the switches above. Most of this fork's 2026-11-27 work is unmeasured in
   game. A report gives: the PC (CPU, GPU and VRAM, RAM), the build (commit), how it was started, whether it was a
   first start, and the fps / 1 % low / ms-per-part figures from the Benchmark page or the log lines above.
2. **A reported regression to explain.** One user got about 4 fps on an RTX 2070 with an early commit of this fork,
   against about 15 fps on another build, with more stutters. Check first:
   - whether `target\diag\victory.exe` was really built from this source, by comparing its date with the commit;
   - whether it was a first start (shader cache rebuild);
   - whether another build shares `%LOCALAPPDATA%\victory\cache`. Early commits swept other builds' caches; this
     is fixed now.
   Then compare the log's ms-per-part figures with a run of the other build.
3. **"Recomp" / write census.** The large remaining win is to make the title's own stores report what they change,
   instead of the layer finding changes afterwards (write watch, guards, heap compares). The first step is a census
   of which store instructions write GPU-visible memory, and how often. It builds on renderer port 2's load-time
   patching (`crates/victory-loader`: `descriptor_sites`, `write_sites`). This needs the game and is per game build.
4. **Bodies and textures through walls, rainbow skin** with the speed set (`-Quality` hides them at about -29 % fps). See `docs/speed-mode-bugs.md`: run `VICTORY_CROSS_WAIT=count` first, then its bisect table; the targeted fixes are `VICTORY_CROSS_WAIT=2` and `VICTORY_TEXTURE_RECHECK=1` (off).
5. **The submitting thread.** The ranked options are in `docs/submit-thread.md`. The big ones need the game to be
   done safely.
