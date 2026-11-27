# What recording and sending host GPU work costs

Where the CPU time of the title's list calls goes once they reach host D3D12, what the driver
itself charges for each call, and the switches to A/B in a game run. All figures here are
**offline** (RTX 4070 SUPER, micro-benchmarks without the title); none is an in-game gain.
The host API is D3D12 only; a Vulkan host would end at the same driver compiler (findings).

## The baseline

`docs/findings.md`: 36,000 list calls a frame, 230 ms of work over eight threads (6.4 us a
call); the README's 6.8 us per graphics call after the renderer ports. With `VICTORY_THIN=1`
(the tuned settings) a call is written down on the title's thread and run through the list
methods on `victory-list-*` threads (`list/thin.rs`); a draw then does `State::prepare` and
`State::bind` (`draw.rs`).

## Where a call's microseconds are not

| Part (offline) | ns a call | Benchmark |
|---|---|---|
| Writing a call down (title's thread) | 5-6 | `bench_thin_call_floor` |
| Running it through the list methods, up to a draw's own work | 9 (23 in a diagnostics build) | same |
| The draw state's lock and bookkeeping (`with_state`) | 7 | same |
| ... plus per-call timing in a diagnostics build (`copystat::recorded`) | +55 to +70 | same, `VICTORY_CALL_TIMES=0` |
| `IASetVertexBuffers` (2 views), `IASetIndexBuffer` | 4-9 | `bench_redundant_sets` |
| `Set*Root32BitConstants` (4), a root CBV, a root signature | 8-20 | same |
| `DrawIndexedInstanced` | 8-14 | same |
| `SetPipelineState` | 60-120 | same |
| `ResourceBarrier` (one barrier, one call) | 100-190 | same |

So of ~6.8 us a call, the stream, the lock and the driver's calls are well under 0.2 us. A
draw records 5-8 host calls, about 50-100 ns of driver time. The rest is the layer's own
work inside `prepare`/`bind`, on title data this benchmark cannot make: per used table
entry a descriptor mirror (`HostHeap::mirror`, `make_at`) and `readable` (texture state,
depth twin, buffer refresh); per vertex/index buffer and root view `shadow::find_any` and
`Shadow::refresh`; textures picked by index (`early_textures`); `memory_ready` for shaders
that read memory by address; target states (`ensure_state`, `twin_use`, the list's state
map under a lock). `VICTORY_DRAW_PARTS=1` (below) times exactly those parts in a game run.

## Redundant state sets

Every draw set its vertex buffers, index buffer, user data (one constants call per stage) and
root views again, also when the draw before had set the same. Measured: 6,500 draws of a
typical shape make 36,225 calls; 21,000 of them set what the list holds already. Leaving them
out records the list 9-13 % faster: about 10 ns a draw, 0.07 ms a frame. Pipelines, root
signatures and heaps were already set only on change (`host_pipeline`, `host_roots`,
`host_heaps` in `State`); render targets too (`VICTORY_BIND_CACHE`, on).

**Done, off by default (`VICTORY_SET_CACHE=1`)**: `list/sets.rs`. A set is left out only when
the host list holds the same value in that slot: nothing is known at a list's start, after a
cut (`State::replay`) or after a root signature (clears the side's arguments) or heaps are set;
`State::rectangles` and `dispatch`'s single word tell it what they set. Off is the old call
sequence byte for byte (`off_every_set_is_made`). It stays off because the gain is small: worth
switching on only if a game run shows it.

Tests (`list/sets/tests.rs`):
- `left_out_sets_change_nothing_a_draw_sees`: a model of what a D3D12 list holds, given every
  set and given only those let through, is equal at every draw of five fixed-seed streams of
  7,000 draws (root signatures, cuts, single-slot sets, words set elsewhere); a blake3 hash of
  the per-draw states matches; over a quarter of the sets are left out.
- `left_out_sets_draw_the_same_picture`: on the card, 3,000 additively blended draws with two
  root signatures draw byte-identical pictures both ways (two seeds), with under 3/4 of the
  calls; a cache that leaves out too much draws another picture (the test can tell).

Note: on this driver the root constants survived a switch between two root signatures of the
same layout (a cache that forgot root signatures drew the same picture). D3D12 does not promise
that; the cache sets them again anyway.

## Barriers

- One barrier in one call costs 100-190 ns, the dearest call after `SetPipelineState`.
- Redundant transitions are already not made: `HostList::transition` keeps each texture's state
  in the list and records nothing for a move to the state it has; the first use goes to the
  prelude at submission.
- Joining calls with no work between exists: `VICTORY_JOIN_BARRIERS=1` (`host/rec.rs`, off). A
  replay saved 2.4 ms of 79.5 on the card (docs in the file). It changes what the card gets, so
  it stays off until a game A/B.
- Split barriers: the title's begin-only halves are dropped and the end half transitions
  (`host_barrier`); real split barriers would need per-subresource tracking. Not done.

## Submission

`bench_submission_calls`, 30 submissions of four lists (50 draws each), CPU time to send:

| How | us |
|---|---|
| One `ExecuteCommandLists` and one `Signal` per submission (`Host::submit` without cuts) | 1,300-1,390 |
| One call per list, a signal per submission | 3,400-5,200 |
| One call per submission, one signal at the end | 280 |
| 30 signals alone | 27 |
| All 120 lists in one call, one signal | 36-53 |

A call followed by a signal is where the driver hands the work to the kernel: about 43 us each
on the submitting thread, the 48 ms one. With `VICTORY_JOIN_PARTS` the parts of a joined
submission still go in calls of their own, and the compare list in one more, so each extra
call is ~25 us. Joining those calls was **not** done: buffers decay to COMMON at the end of an
`ExecuteCommandLists`, not between lists of one call, and the code relies on that between parts
and before the compare list (`host.rs`, "Buffers decay to COMMON between ExecuteCommandLists
calls"). Doing it needs explicit moves to COMMON at each part's end and a debug-layer run.
Expected saving: up to ~25 us per joined part, about 0.5 ms a frame on that thread.

## Switches to A/B in a game run

| Switch | Default | What it does | What to watch |
|---|---|---|---|
| `VICTORY_DRAW_PARTS=1` | off | Times each draw/dispatch by part on the translating threads | new line "draw and dispatch parts": us a call by part; read it first |
| `VICTORY_SET_CACHE=1` | off | Leaves out sets the host list holds (A/B-able with `VICTORY_AB`, one answer per recording) | "host sets per frame": made vs left out; "list calls translated ... in N ms"; the picture |
| `VICTORY_CALL_TIMES=0` | on | Diagnostics builds stop timing every list call | "list calls translated ... in N ms" falls; the log's list-call ms read 0 then |
| `VICTORY_JOIN_BARRIERS=1` | off | Barrier calls with no work between become one (existing) | "barrier calls per frame"; GPU ms; the picture |
| `VICTORY_JOIN_PARTS=<n>` | 1 (tuned: 8) | Parts of submissions prepared as one (existing) | submitting thread ms |

Reading `VICTORY_DRAW_PARTS`: the parts sum to a call's time on the translating threads. Large
"tables" means descriptor mirroring and texture states per entry; large "vertex/index buffers"
or "root views" means host copies refreshed per draw; large "picked textures" the texture pass;
"target states" the state map and depth twins. That says which of them to work on next.
