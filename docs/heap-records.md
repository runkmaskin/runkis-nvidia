# The heaps' records: why the submitting thread waits

What a heap-sync round does, why `victory-submit` waits for it at the end of its preparing, what the heap buffer
pass and retirement cost, and what was changed. Lines are those of this commit. Figures marked *in game* are from
commit 886ae0e (Ryzen 5 5500X3D, 3060-class card, a heavy scene: ~4,600 draws, 69 ms a frame); the others are
*offline* (`cargo test --release`, this PC).

## The round (`crates/victory-gpu/src/descriptor.rs`)

**Who asks.** `Host::submit` (`host.rs:6355`) calls `begin_sync_heaps` (`descriptor.rs:319`) once per submission,
after the card steps and the wait for flights, before the sweep and the preparing. With `VICTORY_HEAP_SYNC_ASIDE`
(on) it adds one to `SYNC_ROUNDS.0` and wakes `victory-heap-sync`. That thread also runs rounds of its own every
4 ms between submissions (`sync_early`, `:194`) for 500 ms after the last asked one. Those are not counted for a
submission. A submission asked during one waits for it to end, then for a whole round of its own.

**What a round does** (`sync_heaps`, `:442`), per shader-visible heap the title placed:
1. *Those that waited* (`:481`): records that waited `COPIED_GRACE` frames for a draw to read them through a table
   and none did (`copied_before`). They are mirrored, since a shader may pick them by index.
2. *What to look at*. With renderer port 2 (`VICTORY_PORT_DESCRIPTORS=1`, the script's setting) these are the runs
   of slots the title's patched stores and our calls marked (`port_descriptors::take`, `:509`), plus up to 16 pages
   the write watch said were written, once a frame (the fallback, `:517`). Without port 2 they are the pages the
   write watch says were written.
3. *Pages read and compared* (`:579`), on the `victory-records` pool (6 threads, jobs of 8 pages): each record is
   compared with the one last seen (`Copied::changes_in_place`). A new record is left to the draws if a recent
   draw's table named the slot (`copied_tabled`). It waits if it is a buffer or placed view (`out.wait`). It is
   skipped if it is no view and the slot shows none. Otherwise it is to be made.
4. *Made* (`:729`): `HostHeap::mirror` for each such record, on the pool. It makes the host descriptor and gives
   the slot's old resource to `host::retire_resource`.
5. The waiting records are queued (`copied_wait`, `:738`).

The heap buffer pass and the sweep are **not** part of the round. The sweep (`host.rs:416`) runs on the submitting
thread at most once a second. The heap pass runs once a frame from the draws (below).

**Why the submission waits.** The round writes the host heap's descriptors, and the card reads them when the lists
run. Mostly it serves slots that no draw's table made: those a shader picks by index (SM 6.6), and old records.
They must be in place before `ExecuteCommandLists`, so the drop of `SyncedHeaps` (`:417`) blocks at
`host.rs:6528-6538`. Nothing in the preparing reads them, so the work runs beside it, and only the round's length
past the preparing is waited for. The frame line's "at the end ... waiting for the heaps' records" (`SPLIT[15]`,
*in game* 10.2 ms, 4.2-25.5) is that wait **plus** `make_waiting` (`:1602`), which makes the slots that waited for
a texture's host copy. The old lines could not tell the two apart.

**Why it is ~10 ms when the round is ~11 ms a frame.** If rounds overlapped the preparing, the wait would be near
zero. Three causes fit the figures, and the game run must decide between them:
- the round starts late: it stands behind an early round, or its thread (normal priority) and the six helpers do
  not get a core on 6 cores / 12 threads with the eight list threads and the title's busy;
- a few submissions a frame carry nearly all of the records (the first after the title rebuilt its tables) and
  have little preparing after `begin_sync_heaps`;
- `make_waiting` is a large part of the 10.2 ms.

The new line (below) gives each of these its own figure.

## The heap buffer pass (`draw.rs`)

`heap_buffers` (`draw.rs:2039`) runs once a frame at the first list whose shader picks buffers by index. With
`VICTORY_HEAP_PASS_ASIDE` (on) it hands `heap_buffer_pass` (`:926`) to `victory-aside` (`:2071`), and the list waits
only at its close. It then runs `heap_textures` itself (`:2083`). **"heap buffers ... passes 13.2 ms"**
(`HEAP_BUFFER_MICROS`, `:2089`) is time on the recording thread. With the pass aside, that is mostly the **texture
side**. Look at "the pass over the heap's views per frame" (looking / spans / textures / waited at close) before
blaming the buffer part.

The pass did not re-scan every slot: it looks at the written slots (`buffer_changes`) plus a sixteenth of the others
(`VICTORY_HEAP_SPREAD`, on), and every slot is looked at again once in 16 passes, as before. What it did redo every
pass was **the spans**: one hash-map entry per host copy, built from all ~72,000 slots, even when no slot changed.

**Changed (exact, no switch).** `KeptOf::pass` (`:619`) notes whether any slot's range moved: a written slot that
differs from before, or the spread part compared as a whole with what it held. `KeptOf::spans` (`:660`) reuses the
last spans if nothing moved. They are kept per host heap serial in `RANGES` and go with `forget_heap`. The spans,
their order, the views and the refreshes are those of the old code. The `unwatched` and `left` checks still run live.
- Test `draw::heap_pass_tests::the_kept_spans_are_the_spans_made_again`: the old code kept as the reference, 5 fixed
  seeds, spread and not, 200 passes each with told, untold, double and copy-replacing writes, and quiet passes. Same
  spans in the same order, the same views and slots each pass, and the same slots kept.
- Benchmark `bench_heap_pass_spans` (72,000 slots, 4,400 copies, offline): a spread pass that moves no range went
  from 0.80 to 0.29 ms. A pass whose ranges move costs what it did. Expect a small gain: about 0.5 ms a frame on
  `victory-aside`, not on the critical path.

## Overlap

- **Round N+1 during N.** Without `VICTORY_HEAP_SYNC_GPU` it cannot start: the thread is blocked in N's wait. The
  early rounds already do N+1's work ahead. The next safe step is to ask at the top of `Host::submit`, before the
  card steps (7 ms), and ask again after any flight settle (a landing writes title memory). The submission then
  waits for the last round asked, so no round starts before a point the old one followed. That is an edit to
  `Host::submit` (not this work's file). It belongs behind a switch.
- **Only this submission's slots.** Not safe: the round's work is exactly the slots no draw's table names, i.e.
  those read by index, and nothing says which index a shader computes. A submission with no list that ran
  `heap_buffers` might skip the wait. That needs a per-list flag at the call site and a game check.
- **`VICTORY_HEAP_SYNC_GPU=1`**: the queue waits on a fence the round signals (`SyncedHeaps::defer`, `:391`; forced
  after 400 ms). The thread goes on, so round N+1 can be asked while N runs. The card then idles for whatever of the
  round is left. **`VICTORY_PORT_DESCRIPTORS_APART=1`** cuts the fallback pages around the marked runs, so each
  slot is compared once a round: fewer compares, and no slot compared by two helpers at once. They compose: APART
  makes the round shorter, whoever waits for it, and GPU moves the wait off the thread. With GPU on, a long round
  shows up as card idle time and in `SYNC_FORCED`, not in the frame line's wait. Measure APART first.

## Retirement (`host.rs:448-540`, `host/retire.rs`)

`retire_resource` had no allocation per call (`Retired::Resource` has no box). It is O(1). `bury` takes what is
due from the front (O(dropped)) and drops it outside the lock. The cost was sharing: 20,000 pushes a frame from the
title's threads and both helper pools onto one mutex, one queue tail and one `RETIREMENTS` counter, so each push
moved those cache lines between cores (*in game* 1.8 % exclusive, 2.6 % inclusive of the list threads).

**New, off: `VICTORY_RETIRE_APART=1`.** Each thread pushes into its own queue (`retire::Graves`, 16 queues, each on
its own cache line) and counts there. The counts reach `RETIREMENTS` at the next bury. `bury` takes the due front of
each queue. An object is still dropped only once the submissions it waits for have finished, with `VICTORY_BURY_LAG`
as before. One thread that overtakes another now holds back only its own queue, so some objects go a little sooner,
never early. No reuse of retired resources: the code has no such pattern.
- Tests `host::retire::tests`: against the one queue (fixed seeds, 1-12 real threads overtaking each other),
  nothing is taken before its submission has finished, and everything the one queue lets go by a bury is gone by
  the same bury. All goes in the end. A race test (8 threads retiring while another buries) drops nothing early.
- Benchmark `bench_retire_apart` (8 threads x 20,000, offline, worst-case contention): 61-89 ns a push into the one
  queue, 4-20 ns into queues kept apart.

## New line, every 5 s (diagnostics builds and plain)

`the heaps' records' rounds per frame: N for submissions of X ms each, begun Y ms after they were asked for; M early
of Z ms each; the submitting thread waited A ms for them and B ms making slots that waited for a texture`.
A + B is the "at the end" figure. Large Y: an early round in front, or no core for the thread (try
`VICTORY_POOL_PRIORITY=1`, `VICTORY_HEAP_SYNC_EARLY=0`). Large X with few submissions: the records come in bursts.
Large B: `make_waiting`, not the round.

## What to watch in a game run

Same scene, interleaved:
- **This build, switches off.** The new rounds line, to split the 10.2 ms. "the pass over the heap's views per
  frame": the spans part ("their copies' spans and refreshes") should be lower than on 886ae0e; looking and
  textures unchanged. The picture the same.
- **`VICTORY_RETIRE_APART=1` against off.** The list threads' and records' time in `retire_resource` in a profile
  should fall. "the heaps' records" ms and "waiting for the heaps' records" no higher. `memory by subsystem`: no
  growth. No device removals: if one comes, the context line still counts retirements.
- **`VICTORY_PORT_DESCRIPTORS_APART=1`, then `VICTORY_HEAP_SYNC_GPU=1`** (each alone, then both): "preparing per
  frame, the submitting thread: at the end ... waiting for the heaps' records" should fall. For GPU, also fps and
  "on the GPU", and no "took over 400 ms" warning.

## Asked early (`VICTORY_HEAP_ROUNDS_EARLY=1`, off)

The rounds line of 8d8db49 (in game): 17 rounds a frame, begun 0.3 ms after the ask, 0.5 ms each, and 10.7-13.3 ms
waited. The wait was the wake-up and the round, one after the other. With the switch, `Host::submit` asks a round
at its top as well, and at the wait a round nobody has begun runs on the submitting thread
(`descriptor/rounds.rs`). The wait still ends only after a round that began after the old ask, so the records a
submission sees are those it saw. The changes file has the tests and the offline figures. The line to read is
"the heaps' records' rounds asked early", printed just before the rounds line.
