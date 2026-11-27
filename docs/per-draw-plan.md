# Per-draw cost on the submitting thread: a plan

Design only. Nothing here was measured for this note. Figures come from `docs/findings.md`, `docs/changes-2026-11-27.md`,
`scripts/play-nvidia.ps1` and code comments. **(est.)** marks an estimate.

## 0. Where this starts

- Friend's run, RTX 2070: 14.4 ms of frame and about 9.3 µs of "preparing" per draw. Testers with faster cards stop near
  25 fps. That is the `victory-submit` thread (`queue/behind.rs`, `Host::submit`, host.rs:5967).
- Local, port 5 (`VICTORY_PORT_CBV=1`): preparing went from 9.4 to 7.7 ms per 1,000 draws (play-nvidia.ps1:87). So the
  constant-buffer fills cost about **1.7 µs a draw**. It is off because the 2070 is GPU-bound and port 5 adds GPU work.
- 7900 XTX, 48 ms on this thread (findings): list steps 18 (refreshes 10), card steps 7, heap records wait 6, write
  watch 5, copies in card memory 4, title bytes under flights 4.
- Counts in code comments: 20,000 refreshes of 4,500 copies a frame (host.rs:3305); 13,000 copies and fills
  (host.rs:7747); 23,000 constant fills in the city (port_cbv.rs:9); 2,800 fills done on the card (card_steps.rs:704).
- **The main fact.** With `VICTORY_THIN=1` (on in the play set) the draw itself already runs on the thin threads:
  binding, tables, dynamic slots, making descriptors, pipeline lookup and recording into the host list. What reaches the
  submitting thread is the list's **steps** (`Step` in `Deferred.copies`):
  - one `Fill` per constant buffer per draw;
  - one `Refresh` per buffer range read (deduped per list);
  - one `Texture` and one `Host` closure per texture new to the list;
  - one `GpuWrite` per UAV range.

  The serial cost per draw is steps per draw × serial passes over the steps × cost per visit, plus the content work.
  About 8 steps per draw (est., 33,000 steps a frame over some 4,000 draws).

## 1. One draw, step by step

S = serial on `victory-submit`. P = parallel. Shares are the 7900 split of 48 ms scaled to 9.3 µs (est.). The 2070
must be measured (section 4). Rows 9 and 10 fall outside the frame line's "preparing", which starts at host.rs:6049.

| # | Step | Code | Thread | Share of 9.3 µs | Measured by |
|---|---|---|---|---|---|
| 1 | List call written down | thin.rs:108 `put` | title, P | 0 | `recording per frame: N list calls` |
| 2 | Translation, pipeline lookup | thin.rs:264 → draw.rs:1379 `prepare_for`, :1383 `host_pipeline_for`, :1473 `bind` | thin, P | 0 | same line, `ms waiting for pipelines` |
| 3 | Descriptor tables, fresh tables | draw.rs:3226 `tables`; key hash :3369; `host.dynamic` descriptor/host.rs:1843; `make_at` :2747; `constant_at` :3005 | thin, P | 0 now; leaves 1 `Fill` per CBV (draw.rs:3536 → host.rs:7338) | `dynamic slots: N tables asked … ms searching` |
| 4 | Reads become steps | `readable` draw.rs:4342 → `Shadow::refresh` shadow.rs:3770 → `defer_refresh` host.rs:7354; UAV → host.rs:7374; textures → texture.rs:749, :939 | thin, P | 0 now; leaves steps | `copies per frame: N refreshes` |
| 5 | Recording into the host list | draw.rs:1422, :1612, :3670 | thin, P | 0 | - |
| 6 | Close: heap pass beside | host.rs:6717, host/aside.rs | aside, P | 0 unless waited | `… ms waited for at the lists' close` |
| 7 | Submit thread waits for translation | behind.rs:439-442 `Recording::wait` | S wait | unknown | `that thread worked X ms` vs frame; timeline |
| 8 | Lock, `cut_for` | host.rs:5975, :5654 → card_steps.rs:463 | S | small | split `waiting for the submission lock` |
| 9 | Card steps (does, plan, holders, slots) | card_steps.rs:683 → `does_of` :415 (all steps), `plan` :258, `holder`/`mapped` :719-741, `constant_slot` :835-844 | S (slots P at ≥256) | **1.2-1.6** | split `planning steps done on the card`; card_steps.rs:918 line; spans `steps on the card: *` |
| 10 | `late_blocked` / `blocked_by` | host.rs:5994, :3226 (all steps) | S | small | split `waiting for flights` |
| 11 | Heap round begun, sweep | host.rs:6010, :6032 | S | small, per submission | split `the heaps' records and what is swept` |
| 12 | Write-watch asking | spans walk host.rs:6068-6082; `ask_once` watch.rs:547 | S + kernel | **0.8-1.0** | split `asking the write watch (… finding what to ask)`; `kernel askings` line |
| 13 | Title bytes under flights | `stale_under_flights` host.rs:3299 (all refreshes) | S | **0.6-0.8** | split `putting the title's bytes under flights' writes` |
| 14 | Refreshes in front | host.rs:8575: gather (wide at ≥4096 steps with port 6, :8583; else serial :8619), `nothing_new`, updates :8694 | gather S, copies P | **1.0-1.9** | split `refreshes X in N flushes … known to copy nothing`; spans `refreshes in front: *` |
| 15 | The lists' steps | `run_deferred` host.rs:7744: every step; `Fill` :7776 (wide at ≥96, :8784); `Batch::add` :8499 per refresh; `texture_due` :8561; `GpuWrite` :7833 → shadow.rs:3856; `Host` closures :7812 | S (fills P) | **1.6-2.0** (fills ≈1.7 measured) | split `copies and fills X`; `copies per frame: … runs N X ms` |
| 16 | Last refreshes, snapshots | host.rs:6132; shadow.rs:1919 | S / P | small | split `snapshots` |
| 17 | Card copies | `picked::declare`, `declared_writes` host.rs:4108, `card_before` :4561 | S | **0.6-0.8** | split `for copies in the card's memory` |
| 18 | Heap records wait, `make_waiting` | host.rs:6186, descriptor.rs:1549 | S wait | ~1.2, by slots not draws | split `at the end X waiting for the heaps' records` |
| 19 | Send, flight noted, lists landed | host.rs:6227, :6245-6250, :5684-5706, :5725 `land` | S | ~0.4 | split `sending the lists`; span `landed lists reset and given back` |

Row 14 has a side issue. With `VICTORY_PORT_CONSTANTS=1`, copies in place still get `Refresh` steps, because
`defer_refresh` does not check `place`. Every serial pass walks them, and then `update` and `nothing_new` return at
once (shadow.rs:4580, :4620).

## 2. What needs submission order, and what does not

**Ordered.** Each of these needs the submission's own moment:
- **Title memory contents.** Fills, refresh copies and texture reads must read at or after submit. thin.rs:20-28
  allows looks *later* than the call, never earlier.
- **The write watch.** An asking resets the watch, and every refresh must follow the asking it uses
  (submit-thread.md #3).
- **The air** (`line().written`, set at host.rs:5689). It sets `asked` in `does_of`, `blocked_by`, the `Placed`
  check (:7793), `stale_under_flights` and the card-steps plan.
- **Per-copy state.** Pending ranges, snapshots, flights' declares (shadow.rs:3865) and card sends are per copy, not per
  submission (the lock's comment, host.rs:5972).
- Fences, the heap round, `make_waiting`, the sweep.

**Fixed once the list is closed.** These are pure functions of the list's own steps, which nothing changes until
reset. A list submitted twice keeps them.
- The `does_of` skeleton without `asked` (kinds, ranges), plus the union of its read ranges.
- `list_front` (host.rs:8207). It is already per list and joined by `join_fronts` (:8295, test :8317), but today it is
  made at submit time.
- The span of `Refresh` ranges per copy, used by `ask_once` and by `stale_under_flights`. `asked_ahead()` stays a
  per-copy filter at submit time.
- **Segments**, cut at the steps that write title memory (`Copy`, `Run`, `Placed`). Inside a segment, `Fill`, `Refresh`
  and `Texture` only read title memory and write our own, so they commute. The one exception: `GpuWrite` against a
  `Refresh` of the same copy (`flush_one`, :7839), whose order per copy is kept.
- Fills as a flat array per segment; the list's textures; `GpuWrite` ranges; `texture_backs`; `writes`.

The thin threads can build all of this where translation sees `CLOSE` (thin.rs:269, before `done` is set). Without
thin, `HostList::close` can build it.

## 3. Stages, smallest safe stage first

All switches are off by default. Off must be byte-for-byte the old path.

**Offline check for every stage.** A test with fixed seeds, as the `front_spans` tests do. Random step lists and
random air go through the old code and the new. The ordered **effect journal** must be equal: refresh ranges per copy
at each flush point, the set of fills per segment, the texture set, the order of `GpuWrite`s and title writes, the
card-steps ops. Add a `--release` benchmark that prints before and after.

**In-game check for every stage.**
- A `=check` mode builds both answers and logs mismatches. It must stay at 0.
- One run with `VICTORY_UPCOPY_CHECK=1` must show no new messages.
- The picture must be identical: one screenshot a second, same scripted keys (`VICTORY_KEYS`).

| Stage | What moves where | Serial saving (est.) | Risk | Switch | Log figures that must change |
|---|---|---|---|---|---|
| **0** A/B, no code | Port 5 on the fast-GPU PC; heap round waited by the card | ~1.7 µs/draw (measured locally on prep); heap wait up to ~1.2 | none / medium (CPU-signalled fence) | `VICTORY_PORT_CBV=1`, `VICTORY_HEAP_SYNC_GPU=1` (exist) | preparing per draw; `waiting for the heaps' records`; port 5's line |
| **1** Refreshes of copies in place kept apart | `defer_refresh` puts in-place copies in a per-list `placed_reads` vec: holds the `Arc`, `does_of` still sees them | 0-1.0 (depends on their share, unknown) | low | `VICTORY_PLACED_REFRESH_APART=1` | `refreshes N` down by the in-place count; `known to copy nothing` down; `finding what to ask` down |
| **2** (a) The list plan at close | Section 2's summaries built at thin.rs:269. `submit` uses them: per copy in steps 12-14, per list in step 10, the `asked` of step 9 from the read-union first (a list whose reads miss the air needs no per-step test). `run_deferred` runs per segment: `fills.extend`, one `Batch::add` per copy, textures per list | 1.5-3.0 (the 5-7 walks of ~8 steps a draw) | low-medium: a step pushed after the plan is built (aside refreshes come at close, so build after `HostList::close`); `debug_assert` that the step count equals the plan's | `VICTORY_LIST_PLAN=1` (`=check`) | `planning steps done on the card`, `finding what to ask`, spans `refreshes in front: the steps gathered`, `copies and fills` down; `refreshes N MB` and `title copies` the same |
| **3** (c1) Sender beside | After `make_waiting`: `ExecuteCommandLists` + `Signal`, `Sent` assembly and `land()` go to a FIFO thread. The line push stays under the lock, so N+1 sees N in the air | 0.2-0.6 (scales with lists more than draws) | medium: settles must not wait for a fence value not yet queued (wait for "handed to sender" first) | `VICTORY_SEND_ASIDE=1` | `sending the lists` near 0; timeline: the calls on the new thread |
| **4** (b) Plans kept across frames | Card-steps `plan` and holders reused when the list's skeleton hash, the copies' `id`s and the air's hash equal last frame's for the same title list | 0-0.7 (only the planning in step 9). Constants change every frame, and refreshes already skip unchanged ranges | medium: identity of copies and air must be in the key | `VICTORY_PLAN_REUSE=1` (`=check`) | `planning steps done on the card` down; card_steps.rs:918 line the same |
| **5** (d) Write census, then journal | First: a census of the store sites that write GPU-visible memory (`titlewrite` SITES, the write-fault book), counts and bytes per site, kept outside the repo. Then: patched sites (as `descriptor_sites.rs` / `write_sites.rs`) append to a journal; `ask_once` and `nothing_new` read it for the regions it fully covers; elsewhere the write watch stays | 2-4 + title threads' guard faults (write watch 5/48, much of refreshes 10/48, part of 4/48) | high: one missed store is stale data; per game build | `VICTORY_WRITE_CENSUS=1`, later `VICTORY_WRITE_JOURNAL=1` | census mode: pages the kernel says written that the journal missed = 0 over a whole session, per region, before a region leaves the watch; `kernel askings` down |
| **6** (c2) Two stages in a row | Content reads of N+1 while N's card steps and sends run | up to the remaining serial time | very high (submit-thread.md #2-#4) | `VICTORY_SUBMIT_PIPELINE=1` | only after stage 5 |

Ranked by saving over risk: **0, 2, 1, 3, 5, 4, 6.** Stage 1 is smaller than stage 2, so it goes first in code, but it
saves less.

**What this reaches** (est.). Stages 0-3 take 3.5-7 µs of 9.3, about 1.6-4x. The 4-5x needed for 60 fps with 3,000-6,000
draws also needs stage 5. After stage 2, what is left on the serial path per draw is mostly content: the fills, the
refresh looks and the card-steps slot work. That is ordered, and already spread over the wide threads. A GPU-bound 2070
(17-18 fps in the play-set notes) will show little of any of this. Measure on a fast-GPU PC.

## 4. What to measure first in the game

Use a diagnostics build of the commit, not a first start, the same scene and three 90 s runs on the Benchmark page. The
fast-GPU PC matters most.

| Ask for | Decides |
|---|---|
| Every 5 s line: frame line (`preparing`, `draws/frame`), `preparing per frame, the submitting thread` (all of it), `copies per frame`, `recording per frame` | Fit preparing against draws for µs per draw. Steps per draw = (refreshes + title copies + fills) / draws. The largest split row picks between stage 2 (walks: steps 9, 12, 13, 14 gather) and stage 5 (asking, refresh looks) |
| `submissions prepared by a thread of ours … that thread worked X ms` vs ms/frame | Worked ≈ frame: serial-bound, so this plan. Well under: it waits on translation (behind.rs:439) or the card, and the thin threads come first |
| `VICTORY_TIMELINE=60,3,<file outside repo>`: the `.txt` summary's span sums for `submit: *`, `steps on the card: *`, `refreshes in front: *`, `a step of ours` | Exact time per row of section 1, including rows 8-9, which are outside "preparing" |
| A/B `VICTORY_PORT_CBV=1` (and separately `VICTORY_HEAP_SYNC_GPU=1`) on the fast-GPU PC | Whether 1.7 µs/draw comes back as fps where the GPU has room. If yes, port 5 is the answer for fills and stage 2 can leave them alone |
| Port 1's line (regions in place), plus one new diagnostics counter: refresh steps of copies in place | Whether stage 1 is worth doing |
| `asking about title memory per frame: … kernel askings … ms, pages found written` and `copies per frame: … watches, guard faults` | The size of stage 5's prize, and whether the census is worth the per-build cost |
| card_steps.rs:918 line and `planning steps done on the card` | Whether stage 4 has anything to save |
| `copies per frame: … runs N X ms` | The cost of the `Host` closures that textures add (row 15) |
