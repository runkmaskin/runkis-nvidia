# The thread that prepares submissions

What one submission does, where it runs, what it waits on, and what could move.
Figures are those of `docs/findings.md` (a 55 ms frame on the water, RX 7900 XTX,
about 30 submissions a frame) unless said otherwise; none measured for this note.

## Who does what

- **victory-submit** (`queue/behind.rs`, `VICTORY_ASYNC_SUBMIT=1` or `VICTORY_THIN`):
  takes the title's submissions out of the line (`LANE.serve`) one at a time and
  calls `Host::submit` (`host.rs`) for each. This is the 48 ms thread.
- **victory-heap-sync** (`descriptor.rs`, `begin_sync_heaps`, `VICTORY_HEAP_SYNC_ASIDE`):
  one round of `sync_heaps` per submission, begun early in `submit`, waited for at
  its end. Rounds of its own every 4 ms between submissions (`VICTORY_HEAP_SYNC_EARLY`).
- **The record pool** (`land::each_view(true, ..)`): the round's page compares and
  the making of descriptors, in jobs of 8 pages / 256 records.
- **wide_each** (`shadow::wide_each`): refreshes of host copies ahead of writes,
  textures, fills (96 or more) are spread over it.
- **victory-relay** and the **land pool** (`land.rs`): what shaders wrote goes back
  into title memory behind the flights, not on the submitting thread.
- **victory-ask-early** (`watch.rs`, `VICTORY_ASK_EARLY`): asks the write watch in
  2 MiB pieces ahead of the submission.
- **victory-aside** (`host/aside.rs`): the pass over buffer views, at list close.

## One submission, in order (`Host::submit`, the `late` path)

| Step (timeline span) | ms a frame | Waits on | Depends on the step before |
|---|---|---|---|
| the submission lock | small | the previous submission's preparing | - |
| steps given to the card (`card_steps`) | 7 | nothing | plans from the lists only |
| waiting for flights it reads from | in "waiting" | flights in the air | yes: order of memory |
| `begin_sync_heaps` (asks a round) | 0 | - | no |
| the sweep | small | nothing in the air | yes: frees what no list names |
| asking the write watch (`ask_once`) | 5 | the kernel | must precede the refreshes |
| title bytes under flights' writes | 4 | flights (`settle_stale`) | needs the watch's answer |
| refreshes ahead of writes | part of 10 | wide_each | needs the watch's answer |
| the lists' steps (`run_deferred`, fills) | 18 (refreshes 10) | wide_each | in list order |
| the last refreshes, snapshots | in the 18 | - | after the steps |
| copies in the card's memory (`card_before`) | 4 | - | after snapshots |
| waiting for the heaps' records | 6 | the heap-sync round | the round only |
| `make_waiting`, sending, noting the air | small | the driver | after everything |

The heap-sync round itself: ~11 ms a frame of its thread and pool (README), 400,000
slots looked at and 40,000 mirrored (findings). It reads the title's heap and writes
host descriptors; nothing the submitting thread does between `begin_sync_heaps` and
the wait reads those. That is why the 6 ms is a wait and not work.

## What has no data dependency and could overlap

1. **The heap-sync round** is already beside the preparing. What is left is its
   length past the preparing (6 ms). It shortens if the round looks at fewer slots,
   or the card waits for it instead of the thread (`VICTORY_HEAP_SYNC_GPU=1`, exists,
   off; untested on this fork's game).
2. **`card_steps` planning** (7 ms) reads only the lists' recorded steps. It could be
   done for submission N+1 while N's steps run. But it settles what is in the air
   for N+1, which N changes: it is in order with N's sending.
3. **The write-watch asking** (5 ms) is ordered: an asking resets the watch, and a
   refresh must follow the asking it used. Asking N+1's ranges during N's steps would
   let N's refreshes miss writes made between. Already partly early (`VICTORY_ASK_EARLY`).
4. **Splitting a submission over two threads in a row** (stage one: lock, card steps,
   settle, watch; stage two: steps, refreshes, card copies, send): the host copies,
   snapshots and shader-write ranges are per copy, not per submission (the lock's
   comment), so stage one of N+1 must not touch what stage two of N still writes.
   That holds for the watch's answer only if each copy's ranges are kept apart per
   submission. Large change.
5. **The card's own buffers in place of fills' copies**: a fill copies title bytes
   into a host copy; viewing the card's buffer means a resource view per fill, and
   the order of the title's own buffer copies against the card's reads changes.
6. **Port 2's fallback** (with `VICTORY_PORT_DESCRIPTORS=1`): once a frame up to 16
   pages the watch said were written are looked at whole. They are the pages the
   marks are on, so their marked slots were in two parts of the same round: compared
   twice, the record last seen of them written by two threads at once (the code's
   own rule is parts apart), and, when the page's part ran first, counted and said
   as "a view record no store told of" (a lock, a `format!`, a map entry and, early
   on, a warning each). This is not the main path, but it is the one place where the
   code looks at slots port 2 already covers.

## Ranked: expected saving over risk

| # | Option | Saves (from the figures) | Risk | Rank |
|---|---|---|---|---|
| 6 | Fallback pages without the marked slots (`VICTORY_PORT_DESCRIPTORS_APART=1`) | up to 32,768 compares a frame and the false misses; under 0.5 ms of the round, which is the 6 ms wait's tail | very low: the same slots are made, tested | 1 |
| 1 | The card waits for the round (`VICTORY_HEAP_SYNC_GPU=1`, exists) | up to 6 ms of the thread | medium: a CPU-signalled fence on the drawing queue | 2 (needs a game A/B, no code) |
| 2 | `card_steps` for N+1 during N | up to 7 ms | high: what is in the air changes with N | 3 |
| 3 | Watch asked ahead for N+1 | up to 5 ms | high: a refresh would miss writes | 4 |
| 4 | Two threads in a row | up to ~20 ms of throughput | very high: per-copy state shared | 5 |
| 5 | Card's buffers in place of fills | up to 4 ms of copies, some of the 18 | very high: memory order | 6 |

Only #6 is safe without the game (done); #1 needs a measured run; #2-#5 change orders.

## What was done (#6)

`VICTORY_PORT_DESCRIPTORS_APART=1` (off by default; read once in
`port_descriptors::apart`). In `descriptor::sync_heaps`, with port 2's heap, the
fallback's pages are cut by `port_descriptors::outside` around the runs taken from
the marks in the same round, so each slot is in one part. Off: whole, as before.

Tests (`port_descriptors::tests`, fixed seeds): the pieces and runs hold each slot of
the pages exactly once; a model of the round's compare made from the old parts
(runs first and pages first) and from the new ones makes the same slots and leaves
the same records last seen over six rounds, and the new one counts only true misses.
Benchmark: `cargo test --release -p victory-gpu --lib -- --ignored --nocapture
bench_pages_without_runs` (2^20 slots, 40,000 marked, 16 pages, 250 true misses):
runs anywhere, 72,005 slots looked at against 70,774 and 1,470 counted missed against
240; runs on the pages, 56,058 against 32,894 and 23,236 against 72. The cut: 1-2 us.

## What to look at in a game run

Same scene, `VICTORY_PORT_DESCRIPTORS=1`, the switch on and off:

- "renderer port 2 ... per frame": "view records no store told of" should fall,
  to the true misses; "pages looked at whole" the same; its "ms" (the fallback's
  time) lower.
- "descriptor records the title copied into its heap as bytes": "slots looked at"
  lower, "mirrored" the same within noise; "ms waited for" no higher.
- "preparing per frame, the submitting thread": "waiting for the heaps' records" no higher.
- With the switch on, a line "renderer port 2's fallback pages without the slots
  told in the same round" gives the slots left out a frame.
- The picture the same: no missing or stale textures.
