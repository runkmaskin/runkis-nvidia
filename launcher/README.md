# Runkis Launcher

A Windows desktop app that sets up, starts, watches and troubleshoots the layer. It is a front end only: it starts
`victory.exe play` with the options and switches you pick, and every setting it shows is one `victory play` or the
layer reads.

Start it with **`LAUNCHER.bat`** in the repository root. The first start builds it into `launcher\dist` (needs
the .NET 8 SDK). Afterwards `launcher\dist\RunkisLauncher.exe` runs on the .NET 8 Desktop Runtime. It uses
`target\release\victory.exe` (or `target\diag`, whichever the profile asks for and is built).

## How it starts a run

1. **Settings.** `victory.exe play <game> [options] --set ... --print-env` prints the start's settings as one line
   of JSON and starts nothing: nothing is asked, fetched or written. These are the settings a double click on
   `victory.exe` starts with (`apps/victory/src/play.rs`), so there is one source. The profile's play options go
   into that call: `--series-s`, `--res`, `--dlss`, `--framegen`, `--fg-type`, `--fps`.
2. **Switches.** The profile's switches go on top as `--set NAME=value`: this fork's `VICTORY_FENCE_SLEEP` and
   `VICTORY_TITLE_POOL`, and upstream's `VICTORY_FULLSCREEN`, `VICTORY_MOUSE_CAPTURE`, `VICTORY_PANEL`,
   `VICTORY_ADAPTER`, pipeline wait, compare threads, render-target room, pipeline book, DRED. An unset field sets
   nothing. "Show effective settings" lists the result, with a mark on what the profile set.
3. **Process.** PLAY starts `victory.exe play <game> [options] --set ...`. `victory play` clears every other
   `VICTORY_*` variable, so the switches go as `--set`, and its own restarts (the first start of a new build plays
   about 10 s and starts again; the F1 menu's "save and restart") keep them. The paths (`VICTORY_DATA_DIR`,
   `VICTORY_CACHE_DIR`, `VICTORY_GAME_DIR`) and `VICTORY_LOG` go in the environment, which `victory play` keeps.
   The run also gets `VICTORY_FRAME_LOG` (one line per frame) and `NO_COLOR`.
4. **Output.** stdout and stderr go to `%LOCALAPPDATA%\RunkisLauncher\logs\<time>-<mode>.log` (the newest 30
   runs are kept) and to the Logs page. `victory play` writes its own log in `%LOCALAPPDATA%\victory\logs` too.
5. **Stop.** The game window is asked to close, so the title can save. If it is still running after 8 s, the
   process tree is ended.
6. **Restarts.** If the title restarts itself, or `victory play` starts the game again through PowerShell (its
   warm-up and menu restarts), the launcher follows the new process. Its output then goes to its own console.

What the in-game F1 menu saves (`%LOCALAPPDATA%\victory\victory-settings.txt`) goes over the play options at every
start, as with a double click: a console or DLSS mode saved there wins over the profile's. Delete that file to go
back to the profile's choices.

## Presets ("Start as" on the Play page)

Each preset is applied over `victory play`'s own set. It keeps your game, folders, card, build, mouse capture,
full screen and frame rate cap, and it can be saved as a profile.

| Preset | What it sets |
|---|---|
| **Recommended for this PC** | `victory play`'s set on the NVIDIA card with the most memory; under 7.5 GB of video memory, DLSS Performance |
| Standard | `victory play` as it is: Series X at 2560x1440, DLSS Quality, frame generation ready but off |
| Frame generation | Standard with frame generation 2x from the start |
| Faster | DLSS Performance |
| Series S | The Series S profile with DLSS Quality |
| Fence sleep | Standard with `VICTORY_FENCE_SLEEP=1` |
| Troubleshoot | DRED light (diag build) and the stats panel |
## Where the numbers come from

Nothing is estimated. A figure that is not available shows "—", and its tooltip says why.

| Figure | Source |
|---|---|
| FPS, frame time, 1 % low, hitches | `VICTORY_FRAME_LOG`, every presented frame (flushed 4x a second). The fallback is the caption the layer writes on the game window once a second |
| CPU, RAM of the run | The process's CPU time and private bytes |
| GPU 3D %, VRAM of the run | Windows' GPU counters (`GPU Engine`, `GPU Process Memory`), filtered by process and the chosen card's LUID |
| Card load, VRAM, temperature, clock, power | NVIDIA's `nvml.dll`, loaded in-process. No `nvidia-smi` processes |
| Cards, driver version | DXGI, in the order the layer numbers them for `VICTORY_ADAPTER` |

"1 % low" is the frame rate of the slowest 1 % of frames over the last 10 s.

## Diagnostics

The Diagnostics page checks the following before a run:

- The build exists and is newer than the source.
- The profile is valid.
- `victory config` accepts the folders. It now exits non-zero on a policy violation.
- Free disk space for the cache and for the driver's cache on C:.
- RAM and the commit limit.
- Which card the adapter setting resolves to, using the layer's own rule. A run that would land on an integrated
  GPU while an NVIDIA card is present is a failure.
- HAGS and the power plan (the README's test conditions).
- Whether the launcher runs as administrator.
- Other victory processes still running.

During and after a run, known messages from the layer are explained on the Play page: device removed, commit
limit, unimplemented import, missing DLLs, panic, the policy, the adapter, AVX2. `crashes.log` is read when the
run ends.

## Benchmark

The Benchmark page measures settings on your own PC, by the README's method: the same save, scripted input, a
warm-up, then a measured window read from the layer's own logs. Load the save you want to measure in the game
first (or let the key script get there), so every run starts from the same place.

**Candidates.** A candidate is a preset ("Recommended for this PC" and the others, applied over your profile's
game, folders, card and build), a saved profile, or either with raw layer switches on top: one `VICTORY_NAME=value`
a line, checked against `^VICTORY_[A-Z0-9_]+=.*$`. `VICTORY_FRAME_LOG` and `VICTORY_KEYS` are the benchmark's own
and are refused there. The first candidate is the baseline the others are compared with ("Make first" changes it).

**A run.** Defaults: 40 s warm-up, 90 s measured, 2 repeats. The runs go interleaved, A B A B, so drift over the
session (heat, caches, background work) averages out.

1. victory.exe starts through the same path as Play: `victory play` with the profile's options and switches,
   then the candidate's raw settings and the key script, all as `--set`. Its logs are `logs\<time>-bench.log` and `.frames.txt`.
2. The warm-up counts from the **first picture**: the frame log's second 0, which the layer writes at the first
   present. Shader translation before it does not eat into the warm-up.
3. At warm-up + measured seconds, the run is stopped the normal way: the window is asked to close, and the process
   tree is ended after 8 s. The next run starts only once that process is gone, and never beside another victory
   process (it waits 15 s for one, then that run fails).
4. A run fails, and the next one goes on, when it ends before the measurement is done (a crash: the message names
   the exit and the problems found in the log), shows no picture within warm-up + 120 s of its start, shows no
   picture in the last 10 s of the window (frozen), or has fewer than two frames in the window. Cancel stops the
   current run the same graceful way and starts no other; what was measured is saved, marked cancelled.
5. If the title restarts itself, the new process writes the frame log again; the run's clock starts again with it.

**The figures**, over the measured window only:

| Figure | Source |
|---|---|
| Average fps, 1 % low, median frame time, worst frame, hitches ≥100 ms | The frame log: frames presented after warm-up s and up to warm-up + measured s. Average fps is frames over their total time; 1 % low is the frame rate of the slowest 1 % of frames, averaged, as on the Play page |
| Preparing, on the GPU, copying back (ms per frame) | The mean over the window of the layer's five-second line `{fps} fps, {ms} ms/frame ({cpu} preparing: copies … ms … MB, textures … ms … MB; {gpu} on the GPU; {back_ms} copying back {back_mb} MB), …` (victory-gpu `host.rs`, `Meter::frame`). Each line's figures are of the second before it; a line counts when that second lies in the window |
| Peak private, peak VRAM (MB) | The highest `private` and `gpu_used` of the `memory by subsystem:` lines in the window (`host.rs` `memory_line`). Not the lines' own `_peak` fields, which include the warm-up |

A figure the source did not give is empty in the file and "—" on the page; nothing is estimated. Tests format the
two `host.rs` format strings the way Rust does and parse them back, so a change to either line fails a test.

**Results.** Per candidate: mean ± spread over its good runs, where the spread is half the range,
(max − min) / 2. Each other candidate shows its difference from the first, marked "within noise" when it is under
max(0.5 fps, 2 × the larger of the two spreads). The 0.5 floor is the README's ±0.4 fps between identical runs;
for the other figures the launcher uses floors of 1 ms (median), 10 ms (worst frame), 1 hitch, 0.5 ms (preparing,
GPU, copying back) and 64 MB (memory). With fewer than two good runs on either side, the noise is unknown and is
shown as such. A verdict line per figure sums it up.

Each benchmark is saved as `%LOCALAPPDATA%\RunkisLauncher\benchmarks\<yyyyMMdd-HHmmss>.json` (options, candidates
with their full profiles, every run) and listed under "Past benchmarks". "Export CSV" writes one row per run, then
per candidate its mean, spread, difference from the first and whether that is within noise.

### Key script (`VICTORY_KEYS`)

The layer presses keys from `VICTORY_KEYS` (`crates/victory-hle/src/modules/gameinput.rs`, `scripted()`), with or
without focus. The format, exactly as the layer reads it:

- Entries are separated by commas: `VICTORY_KEYS=30:Enter,35:X,40-70:W`.
- `<seconds>:<key>` holds the key for 0.2 s from that second. `<from>-<to>:<key>` holds it from `from` up to,
  not including, `to`. Seconds are decimal numbers (`2.5`); spaces around them and the key are ignored.
- A key is one letter or digit (either case), or `Enter`, `Esc`, `Tab`, `Back`, `Up`, `Down`, `Left`, `Right`.
  These names are case-sensitive.
- The keys are keyboard keys to the title, and they drive the keyboard's stand-in pad (`fallback()`) as the
  Controls page binds them. By default: Enter A, Back B, Esc Menu, Tab View, X X, Y Y, arrows the d-pad, Q/E the
  shoulders, F/H the stick clicks, W/A/S/D the left stick, I/J/K/L the right stick, Z/C the triggers. By default (`VICTORY_ONE_PAD` unset) the title sees one
  pad fed by the stand-in and every real pad, so all of these work with a pad connected too. With
  `VICTORY_ONE_PAD=0` and a real pad connected, only the buttons reach it (pad 0), not the sticks or triggers.
- **The clock** starts when the title first opens its input (`GameInputCreate`), not when the process starts and
  not at the first picture. A key script has no other time base.
- The layer skips an entry it cannot read, without a word. The launcher refuses to start with one, and names it.

In the launcher, the script can be typed or loaded from a `.txt` file, with entries on lines of their own or
comma-separated and `#` starting a comment; it is passed on as one comma-separated value, shown under the box. The
page warns about keys that come after the run is stopped (warm-up + measured) or are never held (`to` not after
`from`), and says whether any key is held during the measured window. The script's clock and the warm-up's
(first picture) both start during start-up, at moments the launcher cannot see, so their offset is not known in
advance: each run records when its first picture came after the process started, to fit a script after one trial.

## Controls

The Controls page sets what the keyboard and mouse press on the stand-in controller the layer gives the game (the
keyboard's stand-in pad, `crates/victory-hle/src/modules/gameinput.rs`). With the default one-pad mode the game
sees it merged with every real pad. The game reads the keyboard itself too (upstream's base), with its own keys.

- **The table.** One row per pad button, trigger and stick direction, named for the Xbox pad and the PS5
  DualSense. Each row has two key buttons. Click one, press a key or a mouse button; Esc cancels; ✕ leaves the
  row without a key. A key on two rows presses both; the page warns about it but allows it.
- **Mouse camera** (`VICTORY_MOUSE=1`, on by default here): the mouse is the stand-in pad's right stick, the
  cursor held at the window's centre; left button fires (RT), right aims (LT), middle clicks the right stick.
  Upstream's own mouse did not turn the camera in a test on this PC, which is why this is the default.
- **Mouse sensitivity** (`VICTORY_MOUSE_SENSE`, set only when not 1) and **Invert up and down**
  (`VICTORY_MOUSE_INVERT_Y=1`) apply to the mouse camera and to the game's own mouse.
- **The game's own mouse** (`VICTORY_MOUSE_CAPTURE=1`, off): upstream's native mouse with the cursor held in the
  window. With the mouse camera on it is not fed.
- **Save** the profile to keep the bindings (`KeyMap` in the profile; null: the defaults). Presets keep your
  controls.

The run gets `VICTORY_KEYMAP` only when the bindings differ from the defaults; with the defaults nothing is set
and the stand-in pad is upstream's exactly. The value lists every target in order, keys joined by `|`, for example
`A=Space,B=Back,...,LT=Z|Mouse2,RT=C|Mouse1,...`. The tests check the launcher's names and defaults against the
layer's own lists.
## Play mode and Dev mode

A switch at the bottom of the sidebar, remembered in `%APPDATA%\RunkisLauncher\launcher.json` (`DevMode`).

- **Play mode** (the default) shows the pages a player needs: Play, Settings and Controls. The Play page has the
  game, Start as, PLAY, the two warm-ups (below), the fps, frame time, 1 % low and hitch tiles, the frame-time
  graph, and the setup checks that did not pass, one line each. Settings shows the profile, the game, the picture
  and frame rate options (console, Series X picture, DLSS, frame generation and its type, frame rate cap, full
  screen, graphics card) and this fork's switches.
- **Dev mode** shows everything: Benchmark, Diagnostics, Logs, System, the CPU/RAM/GPU/VRAM tiles, Translate
  shaders and Build pipeline cache, folders, build, Performance, Diagnostics (DRED, log filter, panel, warm-up
  limit) and the effective settings.

Hidden fields keep their values; Play mode only hides them.

## Warm-up (Play page)

Both translate every shader into the cache first (`victory shaders <game folder>`), then build pipelines into the
driver's cache (`victory warm`), with the progress bar and the time left. They run with `victory play`'s settings
in the environment, since the translations depend on them.

- **Warm up (what you've played)**, the default: `victory warm --used`, only the pipelines earlier plays drew
  with.
- **Full warm-up (everything recorded)**: `victory warm`, every pipeline the game has created.

`victory warm` goes on where it stopped. The F1 menu's PRELOAD SHADERS (`victory preload`) does both and also
stores the loading pipelines in the pipeline library the runs start with.

## Profiles

Profiles are JSON files in `%APPDATA%\RunkisLauncher\profiles`.

- A profile is written to a temporary file, checked by reading it back, then swapped in. The previous version
  is kept as `.bak`.
- Deleting moves the profile to `profiles\deleted`.
- Imported profiles are validated and never overwrite an existing name.
- **Profiles from the old base** (schema 1, before 2026-11-28) are read as they are and become play options once:
  DLSS from their upscaler, in the mode whose scene is nearest their render height (Series S at 768 of 1080
  lines is DLSS Quality, 720 lines), and full screen from a borderless or full-screen display mode. Their data and
  cache folders are kept (a city save in `DataDir` stays the save). Their picture values (stand-in light, haze,
  skips, the sharp upscale, mip drop) are kept in the file but not used: upstream's renderer is the reference.

The first profile holds `victory play`'s own set.

## Layout

- `src/Runkis.Launcher.Core`: everything that is not UI. Profiles, the launch plan, the session, frame stats,
  hardware, diagnostics, the benchmark (`Benchmark/`: the runner, with an injectable session factory, the key
  script and log-line parsers, the statistics and the store). It is tested.
- `src/Runkis.Launcher`: WPF views and one view model.
- `tests/Runkis.Launcher.Tests`: `dotnet test launcher\RunkisLauncher.slnx`. Includes a real `victory play
  --print-env` and an end-to-end start of `victory.exe`.
