# Frame pacing: from the title's present to the host's

How a frame reaches the screen, every wait on the way, and which of them the title needs. Line numbers are those of
the commit that added this note. Durations are *not measured in game*: they follow from the code and from the
figures in `docs/findings.md` (a 55 ms frame, the submitting thread busy about 48 ms of it). The new frame-log
columns below are there to measure them.

## The clocks

- **The title's game clock is real time.** The title reads QPC itself (`kernel32` is passed through, and `rdpru` is
  emulated as QPC: `victory-loader/src/console_insn.rs:482`). Timestamps the queues hand out are QPC too
  (`queue.rs:833`). A slow frame advances the game by its real length; nothing in the layer adds or repays time.
- **The frame clock** (`frame.rs`) is ours: started by the title's `SetFrameIntervalX` (`frame.rs:45`, 16.7 ms
  until it sets one), ticking once per interval from a fixed start. Ticks are absolute (start plus whole
  intervals), so lateness never accumulates.
- **Timer resolution**: `victory-hle/src/runtime.rs:239` (`use_console_timers`) asks for 1 ms timers and opts out
  of power throttling, so the title's own short sleeps (audio, 4 ms) are kept. Not part of the frame path.

## The path (the tuned settings: `VICTORY_THIN=1`, `VICTORY_RELAY=2`)

`VICTORY_THIN=1` turns the submit line on (`queue/behind.rs:638`); `VICTORY_RELAY` turns flights on with the present
sent ahead (`host.rs` `late_free_present`, stage 3).

1. **Title thread, frame wait** (`frame.rs:168`, `wait`). The title polls the origin event with a zero time-out
   (starts the next frame at once) and blocks on the flip event before each present. A blocking wait sleeps to the
   next tick (`frame.rs:193`) unless a tick has passed since the last wait for that event ended
   (`passed_tick`, `frame.rs:130`): then it ends at once, on the tick that passed.
2. **Title thread, `PresentX`** (`queue.rs:857`). With the line on, the present goes into the line behind the
   frame's submissions and the call waits until at most one present is in it (`behind.rs:814`, `:820`,
   `VICTORY_SUBMIT_FRAMES`, default 1): it returns once the previous frame was handed to the host.
3. **victory-submit thread** (`behind.rs:269` `step`, `:572`) takes the present when everything before it went
   (a queue wait not met by then is forced: a wait is not kept across frames) and calls `show` (`queue.rs:1099`).
4. **`Host::present_over`** (`host.rs:9751`): the copy (and the interface plane, `compose.rs`) into the back buffer
   is recorded and sent as a flight of its own (`execute_own`, `host.rs:9875`), not waited for.
5. **`IDXGISwapChain::Present(0, 0)`** (`host.rs:9899`): flip model (`FLIP_DISCARD`, `host.rs:9937`), 2 buffers
   (`host.rs:9307`), sync interval 0, no tearing flag, no waitable object, the system's default latency of 3 frames.
   The window is the title's, so DWM composes it: no tearing, the newest frame at each refresh.
6. **The frame log and meter** (`frame_log`, `host.rs:9334`).

Without the line (`VICTORY_THIN` and `VICTORY_ASYNC_SUBMIT` unset) steps 2-5 run on the title's thread inside
`PresentX`. With flights but not stage 3 (`VICTORY_LATE_WAIT=1|2`) the present settles everything in the air twice
(`queue.rs:879`, `:881`) and the copy is waited for (`host.rs:9880`); without flights every submission, the copy
included, is waited for when it is sent.

## Every wait, and whether the title needs it

| # | Wait | Thread | Typical | Needed? |
|---|---|---|---|---|
| 1 | Frame wait to the next tick (`frame.rs:193`) | title | 0 at 18 fps (frames are longer than a tick); up to one interval when a frame is early | **Yes**: the title's frame interval. Never waited when the tick has passed (no double wait) |
| 2 | Present waits for the previous present to be sent (`behind.rs:820`) | title | the submitting thread's lead over the title: likely 20-40 ms a frame at 18 fps | **Yes**: it bounds how far the title runs ahead (memory it rewrites, fences). `VICTORY_SUBMIT_FRAMES` exists |
| 3 | A submission waits for room in the line (`behind.rs:751`, 64 submissions) | title | 0 normally | Yes, a bound |
| 4 | Pipeline waits (`pipeline/make.rs:321`, `:364`) | recording / translating threads | 0 when warm; up to `VICTORY_PIPELINE_WAIT_MS` (500) a frame, shared by all threads, once per pipeline | Partly: a draw dropped is worse than a short wait. Diagnose only (owned elsewhere) |
| 5 | The 1.5 s sweep: a full settle of the flights (`host.rs:6035`) | submit | the card's lead (a few ms) once every 1.5 s | Needed by the sweep (nothing in the air while idle objects go); a periodic spike. Not changed (submission code) |
| 6 | Settles for title memory reads, fences without an event (`fence.rs:114`), most lists in the air | submit / title | inside "waiting" of the 5 s line | Yes: correctness of what the title observes |
| 7 | `Present` blocking in DXGI (`host.rs:9899`) | submit | expected 0 at 18 fps; up to a refresh when frames come in a burst, or while the card is 3 frames behind | **No**: the layer bounds the title itself (#2, flights); a late frame gains nothing from it. Switch below |
| 8 | Copy waited for (`host.rs:9880`, paths without stage 3, and saved or listed frames) | title or submit | under 1 ms (one copy) | Yes on those paths: they settle at the present by design |

### Double waiting

- **Frame wait then present (#1 then #2):** not double. #1 sleeps to an absolute deadline; #2 waits for an event the
  submitting thread reaches while the title sleeps. The title's total is max(tick, previous frame sent), not a sum.
- **Frame wait when late:** fixed earlier (on by default; `VICTORY_LATE_WAITS=1` sleeps on to the next tick, as
  before, which cost 17 ms of every 730 ms frame in run 58). The fake-clock tests in `frame.rs` now prove both
  sides: a 55 ms frame never sleeps, a 10 ms frame sleeps to its tick, and after a 4 s hitch the following fast
  frames are paced a tick apart (no burst that would repay lost time).
- **Frame wait and DXGI (#1 and #7):** the frame clock paces the title; sync interval 0 means DXGI adds no vblank
  wait. #7 is the only display-side wait, and only when DXGI's queue or buffers are full.

### CPU-GPU sync points per frame (stage 3, line on)

None at the present itself. The title's thread never waits for the card at a present; it waits for the submitting
thread (#2). The submitting thread waits for the card only at #5 (every 1.5 s), #6 (title reads, blocking fences,
`late_most` lists) and #7.

## Diagnostics: the frame log's columns

`VICTORY_FRAME_LOG=<file>`, one line per host present, space-separated. The first three are unchanged; the
launcher (`FrameStats.TryParse`) reads those and accepts more.

| # | Column | Unit |
|---|---|---|
| 1 | seconds since the log began | s |
| 2 | time since the last present | ms |
| 3 | draws in the frame | count |
| 4 | the title's thread inside its present calls (with the line: wait #2) | ms |
| 5 | the title's thread slept in frame waits (#1) | ms |
| 6 | the presenter sending the back-buffer copy (includes #5 when the sweep falls in it) | ms |
| 7 | the presenter in `IDXGISwapChain::Present`, plus the latency-object wait when the switch is on (#7) | ms |
| 8 | threads waiting for pipelines (#4), all threads together | ms |
| 9 | host pipelines the driver compiled plus shaders translated | count |

Columns 4, 5, 8 and 9 are counters since the last line, so a call that straddles two presents counts in the line it
ends in. Cost: two `Instant::now` pairs per present, two relaxed adds per title call; the counters are read only
when the log is on.

## The switch

`VICTORY_PRESENT_LATENCY=1|2|3` (off by default, read once; `host/pacing.rs`). The swap chain is made with
`DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT`, `SetMaximumFrameLatency(n)` and `max(2, n + 1)` buffers. Before
taking the back buffer the presenter waits on the latency object for what is left of the title's frame interval
since the last present, and not at all when the frame is already late (`latency_timeout`, `host/pacing.rs:127`).
After two presents in a row that found the object unsignalled (the display is behind) it waits up to 100 ms, so the
queue stays bounded. Off: the swap chain and `Present` exactly as before. Sync interval stays 0; the frame
interval and the title's waits are untouched.

## What to watch in a game run

- Column 7 with the switch off: if it is near 0 on every line, #7 does not happen on that PC and the switch cannot
  help. Spikes in it after a hitch are what the switch removes; compare 1 % lows on the Benchmark page with
  `VICTORY_PRESENT_LATENCY=2` against off.
- Column 4 near (column 2 minus the title's own work) means the title waits for the submitting thread: the limit is
  that thread, as `findings.md` says, not pacing.
- Column 5 above 0 at 18 fps would mean frames end early: check the "frame interval" log line.
- Column 6 every ~1.5 s higher than the rest: the sweep's settle (#5).
- Columns 8 and 9 non-zero on a hitch line: a pipeline wait or compile caused it (a first sight). Lines with a long
  column 2 and 0 in 4-9 point elsewhere: texture streaming or the submitting thread's own work.
