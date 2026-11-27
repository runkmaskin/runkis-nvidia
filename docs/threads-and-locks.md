# Threads and locks

The layer's threads, the locks they share with the title's threads, and what each one waits on. The title's
code runs natively: its threads are real host threads, and contention is between them and ours. Figures are
*offline* (`cargo test --release`, this PC: 16 logical processors) unless marked otherwise. None was measured in
game.

## The threads (16 processors; the reference PC has 12)

| Threads | Count | Priority / cores | Waits on |
|---|---|---|---|
| `victory-submit` (`queue/behind.rs`) | 1 | above normal, fast cores | the line's `Mutex<State>` + `work` condvar; 1 ms poll while an entry stands at a value the CPU may write |
| `victory-heap-sync` (`descriptor.rs:327`) | 1 | normal, fast cores | `SYNC_ROUNDS` condvar; the records pool |
| `victory-records` / `victory-views` (`land.rs`) | 6 / 4 (`VICTORY_VIEW_THREADS`) | normal | the pool's condvar |
| `victory-land` / `victory-under` (`land.rs`) | P/4, 1-4 / 6 | normal | the pool's condvar |
| `victory-wide-*` (`shadow.rs:1782`) | P/4, 2-6 | normal | its own queue (spin off by default) |
| `victory-list-*` (`list/thin.rs:323`) | 8 (`VICTORY_THIN_THREADS`) | normal | `READY` + `WORK` condvar |
| `victory-relay`, `victory-late-wait` (`host.rs:3593`) | 1 + 1 | normal | fence events (`WaitForSingleObject(INFINITE)`) |
| `victory-pipelines-*` (`pipeline/make.rs`) | 2/3 P, 2-16, + 2 for screens | lowest / below normal | `QUEUE` + `AVAILABLE` condvar |
| `dstorage N` (`dstorage.rs`) | P (`VICTORY_DSTORAGE_WORKERS`, new) | normal | the scheduler's `Mutex<Lanes>` + `work` condvar |
| `mapping N` (`xmem.rs:1740`) | 0 with `VICTORY_MAP_THREADS=1` (the script), else up to 15 | above normal | `batches` condvar; claims by an atomic |
| `xasync` timer, deliveries, pool | 1, 4 (more when stalled), as many as block | normal | condvars with deadlines |
| the title's threads | its own | as set; idle/lowest become below normal unless `VICTORY_TITLE_PRIORITIES=0` (the script sets 0) | the title's own events |

On 16 processors the pools alone are about 60 threads, more than twice the cores. Most sleep most of the time:
the ones that compete at once in a frame are the submitting thread, heap-sync and its 6 record helpers, the 8
list threads, the title's threads, and streaming's workers while the world loads. The defaults were measured on
the reference PC and are kept; the switches below are for A/B runs.

## Locks on hot paths (looked at)

| Lock | Taken | Held across | Verdict |
|---|---|---|---|
| `land::Pool::state` (`land.rs`) | was once per job, by every helper and the caller | nothing slow | **fixed**: once per batch per helper (below) |
| dstorage `Scheduler::lanes` (`dstorage.rs`) | at each start and finish, each enqueue | `Lanes::start` (a scan of the lanes, few) | short. A submit woke all workers: switch below |
| line `Lane::state` (`queue/behind.rs`) | 3-5 times an entry, by the title's 8 list threads (`push`) and our thread | never `go` | short; `step` scans at most 64 runs. Left |
| pipeline `QUEUE` (`pipeline/make.rs`) | per pipeline queued or taken | nothing slow | `notify_all` per pipeline wakes 12 workers. On purpose (a screen-only worker would leave it); load only. Left |
| `xmem::PHYSICAL` | per mapping call of the title | the host's map/unmap | by design; `PAGES` and `SHOWN` were split off it earlier; mapping pool claims by atomic. Left |
| xasync `Pool::state` (`xasync.rs:170`) | per thread-pool task | `thread::spawn` when no worker is idle | rare after start-up. Left |
| loader `IMAGES` (`threads.rs`) | per thread start and end | the images' TLS callbacks | per thread, not per frame. Left |

## What was fixed

**The pool's numbers by an atomic counter** (`land.rs`, `Pool`; all four pools: land, under, views, records).
Every job was taken under the pool's mutex, so the caller and up to 5 helpers took it in turn between jobs (the
heaps' records come in ~1,500 jobs a frame). Now `next` is an `AtomicUsize`, reset under the lock before a batch
is given; a helper takes the lock once to join a batch and once to leave it. The numbers still go out in order,
each once; a panic in the caller still stops new jobs (`fetch_max`); a job that panics is still caught per job.
Not a switch: no order or result changes.
- Tests: `the_counted_pool_gives_what_the_locked_one_gave` (the old pool kept as `LockedPool`; fixed seeds;
  1-7 threads; batches of 0-300; three callers at once), and the existing `the_pool_calls_every_number_once`,
  `a_panicking_job_reaches_the_caller`.
- Benchmark `what_a_batch_costs` (1,536 jobs, offline, best of 40): empty jobs on 6 threads 89 -> 46-52 us a
  batch, 315 -> 111-128 ns a job lost; jobs of ~1 us: 97-98 -> 90 us; ~10 us: no difference. So: under 0.1 ms a
  frame. It was not the hot spot it looked like.

## New switches (off by default; off is the old behaviour byte for byte)

| Switch | What | Offline result | Watch in game |
|---|---|---|---|
| `VICTORY_POOL_PRIORITY=1` | helpers of `victory-records`, `-under`, `-land` above normal, like `victory-submit` which waits for them | none (needs contention) | "waiting for the heaps' records"; landing's ms |
| `VICTORY_DSTORAGE_WORKERS=<n>` | dstorage workers, 1-64; unset: one per processor | - | load times, `busy_workers` in the debug line |
| `VICTORY_DSTORAGE_WAKE_FEW=1` | a submit wakes one worker per read handed over, not all | **not a win on an idle PC**: 1 read a submit 2-7 -> 10-17 us until done; CPU 5-39 -> 14 us; 4 reads about even | the debug line's new `woken_idle`, stutter while streaming |

Tests: `pool_helpers_run_at_the_priority_asked`, `worker_count_follows_the_processors_and_the_switch`,
`few_wakes_start_every_read_as_all_wakes_do` (both wakes, memory copies and zlib reads from a file with a staging
buffer of three, every byte checked). Benchmark: `few_wakes_cost_less`. The woken-for-nothing count with all 16
woken was only 3-6 a submit, not 15: woken workers queue on the lock and most find a read or arrive after it.

## Spins, sleeps and waits (looked at)

- `queue/behind.rs` `serve`: 1 ms `wait_timeout` only while an entry stands at a value the CPU may write without
  calling us; fence waits are woken by `wake()`. Needed. Left.
- `dstorage` `page_list` (1 ms sleeps up to 1.5 s) and the retries' back-off: error paths. Left.
- `xasync` serialized port: 1 s timed wait, for its end only. Left.
- `victory-loader` `crash.rs:158`: `mwaitx` as a yield. Patched to `pause` at its first fault already
  (`VICTORY_PATCH_WAITS`). Exit path `detach_all`: a bounded yield loop. Left.

## Found in other agents' files (not edited)

1. `descriptor.rs:327` `victory-heap-sync` runs at normal priority while `victory-submit` (above normal) waits up
   to ~6 ms a frame for its round. Proposal: give it the priority `VICTORY_POOL_PRIORITY=1` gives the record
   helpers, under the same switch.
2. `descriptor.rs:280-296` `victory-heap-sync-watch` (only with `VICTORY_HEAP_SYNC_GPU=1`) polls each round with
   5 ms sleeps for up to 400 ms. `SYNC_ROUNDS.1` is notified at every round's end: `wait_timeout` on it until the
   due time would wake it once a round and on time.
3. `watch.rs:964-977` `Claims::wait_for`: spins 64 times, then `yield_now` in a loop while another thread's
   `VirtualProtect` of the block runs (100 us to ms in the world). On 16 busy processors the yield returns at
   once: a busy loop. Proposal: after the spins, `Sleep(0)`/`Sleep(1)` back-off as `change_done`
   (`watch.rs:2894`) does, or a per-block parking lot; measure with the claims' own `spun` count first.
4. `shadow.rs:1760` `spin` (`VICTORY_WIDE_SPIN`): off by default. Left.
5. `host.rs` `LINE`/`LINE_MOVED` (2968, 2980), `descriptor.rs` `VIEWS` (1230) and `watch.rs` `GUARDS`/`MAPPING`
   (1863, 2827) are on hot paths and were not examined here; they belong to the GPU-commands and shared-memory work.

## What to look at in a game run

Same scene, interleaved, each switch alone: `VICTORY_POOL_PRIORITY=1` against off ("preparing per frame, the
submitting thread": "waiting for the heaps' records"; landing's "merging what was fetched"); while the world
streams, `VICTORY_DSTORAGE_WAKE_FEW=1` and `VICTORY_DSTORAGE_WORKERS=8` against off (load time to the first
frame, hitches over 100 ms in `*.frames.txt`, the debug line's `woken_idle` and `busy_workers`).
