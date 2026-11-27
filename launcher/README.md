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

## Profiles

Profiles are JSON files in `%APPDATA%\RunkisLauncher\profiles`.

- A profile is written to a temporary file, checked by reading it back, then swapped in. The previous version
  is kept as `.bak`.
- Deleting moves the profile to `profiles\deleted`.
- Imported profiles are validated and never overwrite an existing name.

The first profile holds PLAY.bat's values.

## Layout

- `src/Runkis.Launcher.Core`: everything that is not UI. Profiles, the launch plan, the session, frame stats,
  hardware, diagnostics. It is tested.
- `src/Runkis.Launcher`: WPF views and one view model.
- `tests/Runkis.Launcher.Tests`: `dotnet test launcher\RunkisLauncher.slnx`. Includes a real run of the script and
  an end-to-end start of `victory.exe`.
