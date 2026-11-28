# Runkis Launcher

A Windows desktop app that sets up, starts, watches and troubleshoots the layer. It is a front end only: it
changes nothing in how `victory.exe` runs beyond the settings you pick, and every setting it shows is one the
layer reads.

Start it with **`LAUNCHER.bat`** in the repository root. The first start builds it into `launcher\dist` (needs
the .NET 8 SDK). Afterwards `launcher\dist\RunkisLauncher.exe` runs on the .NET 8 Desktop Runtime.

## How it starts a run

1. **Settings.** `scripts\play-nvidia.ps1 -PrintEnv` prints the tuned settings as JSON and starts nothing. These
   are the same settings PLAY.bat uses, so there is one source and the two cannot drift apart. The profile's
   switches (`-SeriesX`, `-Quality`, `-Resolution`, `-Adapter`, `-Build`, …) and its `VN_*` values go into that call.
2. **Overrides.** The profile's own overrides go on top: pipeline wait, mip drop, compare threads, render-target
   room, pool discard, pipeline book, panel, log filter. Empty fields keep the script's value. "Show effective
   settings" lists the result, with a mark on what the profile changed.
3. **Process.** `victory.exe` starts with that environment. Any `VICTORY_*` or `VN_*` left in the launcher's own
   environment is removed first. It also gets `VICTORY_FRAME_LOG` (one line per frame) and `NO_COLOR`.
4. **Output.** stdout and stderr go to `%LOCALAPPDATA%\RunkisLauncher\logs\<time>-<mode>.log` (the newest 30
   runs are kept) and to the Logs page.
5. **Stop.** The game window is asked to close, so the title can save. If it is still running after 8 s, the
   process tree is ended.
6. **Self-restart.** If the title restarts itself (a new victory process, while the first one exits 0), the
   launcher follows the new process.

## Presets ("Start as" on the Play page)

Each preset is applied over the script's tuned set. It keeps your game, folders, card, build, controls and window
(display mode, size, monitor), and it can be saved as a profile.

| Preset | What it sets |
|---|---|
| **Recommended for this PC** (the default) | Worked out from this PC, starting from the measured set (RTX 2070 SUPER 8 GB, 12 threads). Changes only what that set chose for its own hardware. **Under 7.5 GB:** 480p, with large textures dropped from 1024 px. **CPU threads:** half of them, at most 8, kept for memory compares (the measured set kept 6 of 12). **Card:** if Windows' default card isn't the NVIDIA one, the NVIDIA card with the most memory is picked. The description lists each step. The result is not measured on your PC: compare it with Standard |
| Standard | The measured PLAY.bat set |
| Max FPS | 480p; hair physics, decal defragmentation and heat haze left out |
| Sharp (10 GB+ VRAM) | 720p, full-size textures, 3 GB render-target reserve |
| Fewer glitches | Quality mode (about âˆ’29 % fps) |
| Series X | The Series X profile at 1080p |
| Troubleshoot | Quality mode, every shader waited for, DRED light (diag build) |

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

1. victory.exe starts through the same path as Play: `play-nvidia.ps1 -PrintEnv`, the profile's overrides, then
   the candidate's raw settings and the key script. Its logs are `logs\<time>-bench.log` and `.frames.txt`.
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
sees it merged with every real pad.

- **The table.** One row per pad button, trigger and stick direction, named for the Xbox pad and the PS5
  DualSense (A/Cross, B/Circle, X/Square, Y/Triangle, LB/L1, RB/R1, LT/L2, RT/R2, View/Create, Menu/Options,
  LS click/L3, RS click/R3, the d-pad and both sticks' four directions). Each row has two key buttons.
- **Rebinding.** Click a key button; it shows "press a key…". Press a key or a mouse button and it is bound. Esc
  cancels (so Esc itself is bound back only by "Reset to defaults"). ✕ leaves the row without a key. A key on two
  rows presses both; the page warns about it but allows it.
- **Keys the layer reads:** letters, digits, Space, Enter, Esc, Tab, Backspace, the left Shift, Ctrl and Alt (the
  right ones are not read), the arrows, F1-F12, and mouse buttons 1-5 (left, right, middle, back, forward). Mouse
  buttons drive the pad only; the game never sees them as keyboard keys.
- **Mouse camera** (`VICTORY_MOUSE`) and its sensitivity are here too. With it on, the defaults add left mouse to
  RT, right mouse to LT and middle mouse to the RS click.
- **Save** the profile to keep the bindings. They are stored in the profile as `KeyMap` (null: the defaults).
  Presets keep your controls.

The run gets `VICTORY_KEYMAP` only when the bindings differ from the defaults; with the defaults, nothing is set
and the layer's mapping is exactly the old one. The value lists every target in order, keys joined by `|`:

```
VICTORY_KEYMAP=A=Space,B=Back,X=R,Y=F,LB=Q,RB=E,LT=Z|Mouse2,RT=C|Mouse1,View=Tab,Menu=Esc,LS=Shift,RS=H|Mouse3,Up=Up,Down=Down,Left=Left,Right=Right,LSUp=W,LSDown=S,LSLeft=A,LSRight=D,RSUp=I,RSDown=K,RSLeft=J,RSRight=L
```

The layer reads it once. Target and key names ignore case. A target it lists has only the keys listed (`Y=` none);
a target left out keeps its default (with `VICTORY_MOUSE=1`, the mouse's roles included). An entry it does not
understand is skipped with a warning in the log, and one info line gives the mapping in force. The tests check
the launcher's names and defaults against the layer's own lists.

## Play mode and Dev mode

A switch at the bottom of the sidebar, remembered in `%APPDATA%\RunkisLauncher\launcher.json` (`DevMode`).

- **Play mode** (the default) shows the pages a player needs: Play, Settings and Controls. The Play page has the
  game, Start as, PLAY, the two warm-ups (below), the fps, frame time, 1 % low and hitch tiles, the frame-time graph, and
  the setup checks that did not pass, one line each. Settings shows the profile, the game, Display (mode, window
  size, monitor, upscale, sharpening), render height, graphics card, and the picture fixes as checkboxes
  (Quality mode, hair physics, decals and heat haze).
- **Dev mode** shows everything as before: Benchmark, Diagnostics, Logs, System, the CPU/RAM/GPU/VRAM tiles,
  Translate shaders and Build pipeline cache, folders, console model, build, Smooth Motion copy, ray tracing, the
  light values, Performance, Diagnostics (DRED, log filter, panel, warm-up limit) and the effective settings.

Hidden fields keep their values; Play mode only hides them.

## Warm-up (Play page)

Both translate every shader into the cache first (`victory shaders`), then build pipelines into the driver's cache
(`victory warm`), with the progress bar and the time left.

- **Warm up (what you've played)**, the default: `-Warm -WarmUsed` (`victory warm --used`), only the pipelines
  earlier plays drew with, those of the latest play first.
- **Full warm-up (everything recorded)**: `-Warm` (`victory warm`), every pipeline the game has created, drawn
  with or not. The pipelines drawn with come first here too.

`victory warm` first names each pipeline by what the driver is given (the bar says "Finding the pipelines that
are the same"), builds one of each, and keeps what it built by that name: after a new build only the pipelines
whose translated code changed are built again. Dev mode's Build pipeline cache is the full one, without the
translation step.

## Display (Settings page)

| Field | Variable (crates/victory-gpu/src/display.rs) | Default |
|---|---|---|
| Display mode: Windowed, Borderless window, Fullscreen | `VICTORY_WINDOW=borderless` or `fullscreen` | Windowed: not set |
| Window size (Windowed only): common sizes below the monitor's, and the monitor's own | `VICTORY_WINDOW_SIZE=WxH` (client area) | the game's own size fitted to the screen: not set |
| Monitor: numbered in Windows' order, with its resolution | `VICTORY_MONITOR=n` (1 = first) | the main monitor: not set |

Borderless and Fullscreen are the same window: no frame, covering the whole monitor. There is no exclusive full
screen. Alt+Enter switches between a framed and a borderless window while the game runs (only with one of the
variables set). Picking Borderless or Fullscreen on a monitor above 1080p ticks **Upscale to the window (sharp)**
(`VICTORY_PRESENT_SCALE=1`): the game draws 1080p, and Windows' own stretch is blurry. It can be unticked again.
The checks warn when a window size is given for a borderless mode, or when the monitor is not connected (the
game then opens on the main monitor).
## Profiles

Profiles are JSON files in `%APPDATA%\RunkisLauncher\profiles`.

- A profile is written to a temporary file, checked by reading it back, then swapped in. The previous version
  is kept as `.bak`.
- Deleting moves the profile to `profiles\deleted`.
- Imported profiles are validated and never overwrite an existing name.

The first profile holds PLAY.bat's values.

## Layout

- `src/Runkis.Launcher.Core`: everything that is not UI. Profiles, the launch plan, the session, frame stats,
  hardware, diagnostics, the benchmark (`Benchmark/`: the runner, with an injectable session factory, the key
  script and log-line parsers, the statistics and the store). It is tested.
- `src/Runkis.Launcher`: WPF views and one view model.
- `tests/Runkis.Launcher.Tests`: `dotnet test launcher\RunkisLauncher.slnx`. Includes a real run of the script and
  an end-to-end start of `victory.exe`.

