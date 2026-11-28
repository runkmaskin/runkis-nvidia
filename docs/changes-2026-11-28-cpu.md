# The CPU side of a frame: old-base ports and the submit path (2026-11-28)

Starting point (the user's F3 panel in the city, Series X 1440p): the submitting thread busy 56 ms a frame;
the log's "preparing" about 30 ms (copies about 11 ms and 43 MB, textures about 1 ms), "copying back" 13-14 ms.
All figures below are **offline** (this PC, 7800X3D) unless marked; nothing here has been run in the game.

## The old base's CPU work, against upstream

| Old-base change (old-u0221) | On this base | Done |
|---|---|---|
| `VICTORY_FETCH_PARTS` (76e89d5; +0.65 fps measured in game on the old base) | absent; `relay_card_back` was still the old base's | ported as it was, off |
| AVX2 changed-runs compare (76e89d5, no switch) | absent (byte walk) | ported, no switch (same runs) |
| `VICTORY_POOL_SETTLE` (+0.45 fps) | **upstream has it, always on**: `pool::room` reclaims both pools once a second | not ported |
| `VICTORY_RAM_TRIM` | **upstream's equivalent**: `residency::pressured` keeps one staging block (none at critical commit), spare scratch chunks trimmed under pressure, `VICTORY_ALLOCATOR_TRIM` | not ported |
| List threads' exact fixes (37e8667) | absent | ported, no switch |
| `VICTORY_TABLE_CACHE` (0548aeb) | absent | ported, off |
| `VICTORY_HEAP_ROUNDS_EARLY` (53d7171): (a) a round asked at the top of a submission, (b) a round nobody began run by the submission that waits | (a) **upstream has better**: `VICTORY_HEAP_SYNC_AT_SUBMIT` (on in `victory play`) asks at the title's submit; (b) absent | (b) ported under the old name, off |
| Submit-thread hot spots (eafd20a) | `card_dirty` visiting touched copies only (`TOUCHED`), the kept `Batch` (`SPARE_BATCH`), one walk for the main path's spans and a cheap hasher (`AddressHash`) **already upstream** | `send_copied` by range, a kept table for `stale_under_flights`, copies in place out of a flush: ported, no switch |
| Write-sites census (`VICTORY_WRITE_SITES`) | **upstream has the same code** (`victory-loader/src/write_sites.rs`, `victory-gpu/src/titlewrite.rs`); the old base changed no code for it | nothing to port. Pitfall from the old run: the list must name the mapped image as the log says (`victory-image-<hash>.dll`), else nothing is patched |
| `VICTORY_FETCH_IN_FLIGHT` (7ef6532) | absent | **not ported**: 2,300 lines on top of three old-base layers upstream lacks (`VICTORY_UP_QUEUE`'s slots, `VICTORY_LAND_SEGMENTS`/`MERGE_SPREAD`'s segment cuts, the tells line), a change to the card's compare pass, and only a model's gain (fence latency -46 %) never measured in game. Worth doing only after `FETCH_PARTS` is measured here |
| The submit thread split in two stages (docs/submit-thread.md #4) | the old base never built it: it was ranked "very high risk" and left | nothing to port |

## Ported

- **`VICTORY_FETCH_PARTS=2..8`** (`host.rs`, `relay_fetch_in_parts`): the relay's fetch of card pieces in parts cut
  where a landing's job ends, each merged while the next copies. Works on the copy queue (`VICTORY_RELAY_COPY=1`, as
  `victory play` sets). Not with the torn-piece check. A line "fetches of card copies' pieces in parts" gives the
  waits. Test `relay_tests::a_fetch_in_parts_holds_every_job_once`.
- **AVX2 `changed_runs`** (`shadow.rs`): used by every refresh of a copy in the card's memory that sends only what
  changed (`take` -> `dword_runs`) and by the merge under flights, so on the submitting thread too. Test
  `back_speed_tests::changed_runs_are_those_of_the_walk_byte_by_byte` (the byte walk kept as reference). Benchmark
  `what_changed_runs_cost`, 400 pages of 64 KiB: floats changed all over 77.7 -> 16.6 ms, stretches 9.0 -> 3.9,
  all of it 14.2 -> 3.1, unchanged 1.36 -> 0.83.
- **List threads** (`descriptor/host.rs`, `draw.rs`, `resource.rs`, `fx.rs`; no switch): FxHash and `WordKey`
  maps, one-lookup `make_listed`, `keep_ref`, batched retirement, per-thread command signatures, `TARGETS_AT` read
  lock. Benchmarks: list views 150,000 records 3.82 -> 0.54 ms, retirement on 8 threads 16.95 -> 1.38 ms,
  `TARGETS_AT` on 8 threads 8.78 -> 2.21 ms, signatures 0.41 -> 0.03 ms. Tests `descriptor::host::list_cpu`,
  `draw::signature_tests`.
- **`VICTORY_TABLE_CACHE=1`**: a list keeps its own views for its recording, whichever thread takes the piece, and a
  mirrored entry read at the list's epoch is left as read (upstream's staging rule kept: a texture waiting for
  staging room is read again). Benchmarks: fresh 30-view tables 13.73 -> 7.17 us a draw (views made 5.87 -> 0.87),
  mirrored 12-view tables 7.86 -> 1.90 us a draw; `HOST_WRITTEN` 5.96 -> 2.40 ms. Tests
  `descriptor::host::table_cache_tests`, `descriptor::tests::mirrored_entries_left_as_read_change_nothing`,
  `texture::host_written_tests`. The old base's `VICTORY_DRAW_PARTS` table counters are not carried (no such line
  here).
- **Submit-thread fixes** (`carded.rs`, `host.rs`, `shadow.rs`; no switch): a CPU title buffer copy upgrades only the
  card copies it reaches (4,000 copies: 54.3 -> 4.3 us a title copy); `stale_under_flights`' table kept per thread
  (20,000 refreshes of 4,500 copies: 497 -> 252 us; it runs on the side paths: the retries of `settle_stale` and
  without the single asking); copies in place taken out of a flush's refreshes before the threads get them. Tests
  `carded::tests::the_register_gives_what_the_walk_reached`, `host::under_tests::the_kept_table_gathers_what_a_new_one_did`.
- **`VICTORY_HEAP_ROUNDS_EARLY=1`** (`descriptor/rounds.rs`): at the wait for the heaps' records, a round asked for
  that the heap-sync thread has not begun runs on the submitting thread (one round at a time, the old numbering, so
  each wait still ends after a round begun after its ask). Off: the thread's loop takes the old branches. Test
  `descriptor::rounds::tests::every_wait_ends_after_a_round_begun_after_its_ask` (real threads, with and without).
  Model benchmark `bench_rounds_here` (17 waits a frame, 0.5 ms rounds, 0.3 ms wake-up): 15.5 -> 10.7 ms waited a
  frame, 19.2 -> 9.1 with no preparing past the ask, 12.9 -> 11.1 with 0.4 ms. The round run there is the thread's
  own time; only the wake-up is saved, and with `VICTORY_HEAP_SYNC_AT_SUBMIT` many rounds are done before the wait,
  so expect less here than on the old base. Line: "the heaps' records' rounds run by the submission that waited".

## New from reading the submit path

- **`VICTORY_COPY_VECTOR=1`** (`safecopy.rs`): every read of title memory for refreshes, snapshots and landings is
  `safecopy::copy`, one `rep movsb`, whose start-up costs short pieces (`copy_costs`: 4 KiB 0.37 us against 0.13 for
  a plain copy). The switch copies under 8 KiB 128 bytes a turn with AVX, in a second routine with its own fault
  exits. Benchmark `copy_by_blocks_costs`, scattered into a buffer the card reads: 64 B 0.024 -> 0.010 us, 256 B
  0.024 -> 0.016, 1 KiB 0.219 -> 0.181, 4 KiB 0.388 -> 0.293; 8 KiB and longer unchanged. Test
  `the_copy_by_blocks_is_the_copy_by_movsb`. At 43 MB a frame mostly in 4 KiB pieces that would be about 1 ms;
  mostly in 64 KiB pieces, nothing.
- Found, not changed:
  - Copies of 64 KiB run at 13-15 GB/s whichever way (memory-bound): the 11 ms of copies for 43 MB (about
    4 GB/s) is not the copy loop. It is the per-piece work around it (write-watch answers, `copied` page walks,
    `card_changed_runs` locks per run) and pieces of a page or less. A copystat profile (`-Build diag`) of the
    city is the next step to tell which.
  - `Shadow::take` (send-changed path) reads each 64 KiB page into a thread buffer, then compares and copies the
    runs: two passes over the page. A fused compare-and-copy in the `safecopy::merge` style would save one pass of
    reading; worth it only if copystat shows `take` large.
  - `nothing_new` takes two mutexes (`pending`, `copied`) per refresh and `ranged` a third; fine per refresh,
    but it is per refresh of every list step.

## To A/B in the game (all off by default; each against plain main at the same spot)

`victory play ... --set VICTORY_X=1` (or the launcher's switches). Read the 5-second lines.

| Switch | Watch |
|---|---|
| `VICTORY_FETCH_PARTS=4` | "copying back X ms"; the "fetches ... in parts" line; fps |
| `VICTORY_TABLE_CACHE=1` | fps and 1 % low; list threads' time; the picture (stale textures) |
| `VICTORY_HEAP_ROUNDS_EARLY=1` | its own line's "waited ... ms in all" against the heaps' line's "waited" with it off; SUBMIT BUSY |
| `VICTORY_COPY_VECTOR=1` | "preparing ... copies X ms"; SUBMIT BUSY |
| all four together | as the old base's best set (25.95 -> 27.14 fps there with TABLE_CACHE + HEAP_ROUNDS_EARLY) |

The no-switch ports (AVX2 runs, list-thread and submit-thread fixes) are in both arms of every A/B: compare plain
main (b98bf65) against this branch with no switch for them.
