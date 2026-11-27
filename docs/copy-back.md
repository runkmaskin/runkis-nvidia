# Copying back: what the figure is, who needs it, what can go

The frame line's `{back} copying back {mb} MB` was 23.7 ms and ~20 MB a frame (up to 35 ms and 38 MB) in a heavy scene
(Ryzen 5 5500X3D, 3060-class card, commit 886ae0e, *measured in game*). This note says what that figure counts,
adds a census that tells how much of it the title's CPU reads, and ships two small changes behind switches. Nothing
here is measured in game; figures marked *offline* come from tests on a dev PC.

## 1. What the figure is (read from the code)

Counters: `COPY_BACK[0]` (microseconds) and `COPY_BACK[1]` (bytes), `host.rs`, printed by the frame line in MB of
10^6 bytes, per frame.

**With the relay** (`VICTORY_RELAY=1` or `2`: the speed set and `-Quality` both) it is the **`victory-relay` thread's
wall time** in `Host::relay_land`, per landing that holds at least one flight, from its start until every flight's
ranges are done with (`copy_back_done`). In order:

| Step | Where | Landing-line name |
|---|---|---|
| which pieces to fetch: compare results, earlier fetches (`Had`), order | `relay_card_back` | "which pieces to fetch" |
| record the fetch list, send it (drawing queue under `QUEUE_ORDER`, or the copy queue) | `relay_card_back` | - |
| **the fetch wait** (`WaitForSingleObject` on the relay's fence); copies in the PC's memory are scanned meanwhile | same | relay line "waited N ms for fetches"; "looking through ... beside them" |
| merge fetched pieces on the `victory-land` pool (`land_card` -> `land::piece`) | same | "merging what was fetched" |
| rest of the scan and the merge of copies in the PC's memory (`land_back`) | `RelayBack::merge` | "... and X ms more", "merging the others" |
| ranges done with | `relay_land` | "ranges done with" |

Not in it: the wait for the card to run the flights (relay line "waited for the card with nothing to land"),
buffer and texture read-backs, `card_done`, telling the title (fence values, bottom-of-pipe writes), list reuse.

**The MB are bytes stored into title memory**: dwords that changed, times four (`land::piece`'s return). They are
not the bytes fetched ("host copies in the card's memory: ... N of them fetched (X MB)") nor the bytes compared. 20 MB stored can mean far
more fetched: a piece is fetched whole (64 KiB) if any byte of it changed.

**Without the relay**, the same counters are the submitting thread's `card_back` (fetch with a wait on the host
fence, merge) and `hand_back`.

**Wait or work?** The relay line's "waited N ms for fetches" runs from the send to the fence and includes the scan
done meanwhile. So: *pure wait = relay fetch wait - "looking through ... beside them"*; *work = which pieces +
merging fetched + the rest of the scan + merging the others + ranges done*. *Offline* the merge runs at memory
speed (6.3 ms per 96 MB, `land::tests::what_landing_costs`), so ~20 MB stored is 1-3 ms of merge; the rest of 23.7
ms is expected to be the fetch wait. On the drawing queue a fetch sent after flight N runs behind everything the
submitting thread sent since, so it waits for later flights' GPU work: queueing, not copying (40 MB over PCIe is
~1.6 ms). Confirm with the two lines above in the next game log.

Why it matters: in stage 2, fences and bottom-of-pipe values are set only after their flight's landing
(`late_signal`), and a landing tells nothing before all of it is in. A title thread that waits on a fence waits
for the whole landing, the fetch's queueing included.

Not copy-back: the profile's `shadow::word_runs_put_avx2` on `victory-submit` is the title's bytes under flights'
writes (the stale-under path, `merge_under_writes`), done on the submitting thread before a submission.

## 2. Who reads what lands

| Class | What the layer knows today |
|---|---|
| (a) read by the title's CPU before the card writes it again | nothing: the write watch and the guards see writes, never reads |
| (b) read by later GPU work through a host copy | the CPU-side steps that read title memory declare it: fills, buffer copies, textures (`blocked_by`), refreshes of other copies (`cross_writes`, `VICTORY_CROSS_WAIT=count` counts them); the card already holds these bytes |
| (c) never read | what is left; it includes bytes landed again by the next flight first |

**`VICTORY_BACK_CENSUS=1`** (off; `land/census.rs`) measures (a) against the rest. When `land::piece` stores new
bytes into a 4 KiB page, one page in 16 (`VICTORY_BACK_CENSUS_EVERY`), at most 64 a frame
(`VICTORY_BACK_CENSUS_PAGES`), gets a one-shot `PAGE_GUARD` if it is private read-write memory. The first access of
any thread, read or write, raises a guard exception; a vectored handler ahead of the guards' own notes the thread
and frame and lets the access run again. Untouched pages get their protection back after 60 frames
(`VICTORY_BACK_CENSUS_FRAMES`). View memory (physical pages) is counted, not sampled: `crate::watch` changes its
protections, and a second writer would race it. Every frame-rate line:

```
copy-back census (...): landed per frame A MB in B pages with a new byte; pages looked at C private (.. MB), D views
(.. MB, not sampled), E other; guarded F, no room G, refused H; of the guarded pages resolved since the last line
(by bytes): the title's CPU first T % (pages in the same frame/the next/2-3/4+ frames ...), our landing first L %,
our other threads first O %, untouched U % (n pages, m of them landed in again); k first touches were writes;
title threads first: [names]; p pages still guarded
```

How to read it:
- **T %** is the share of sampled landed bytes the title's threads touch first: class (a), the bytes copy-back is
  for. Our console calls run on title threads and count here too, which errs on the side of "needed".
- **L %** (the next landing got there first) plus **U %** is what was copied back for nobody's CPU: (b) or (c).
  **O %** is our own refreshes and steps: (b).
- The age bins say how soon the title reads: "same frame" heavy means a fence wait is behind it.
- The C/D split says how much landed memory the census can see at all. If views dominate, the census only speaks
  for the private part, and a read census of views needs a hook in `watch.rs`.
- A first touch that is a write (k) is the title overwriting: also not a read.

Cost while on: two protection calls and one exception per sampled page. A system call handed a guarded page fails
once (the kernel removes the guard and returns an error); shader output rarely goes to one, but do not judge the
picture from a census run.

## 3. Reductions

**Shipped, off: `VICTORY_LAND_SEGMENTS=1`** (`land_segments`, `segment_end` in `host.rs`). The relay lands up to the
first tells (fence signals, bottom-of-pipe writes) behind the flights the card has run, tells them, then lands the
rest. A fence behind the first of four flights no longer waits for the fetches and merges of all four. Cost: one
fetch and its wait per part. Order: the cut is always right before a flight, which is where `landable` cuts when
the card has run only the flights ahead; the relay already sees such lines whenever it is quicker than the card.
Tests: `relay_tests::landings_cut_at_tells_cut_where_the_card_could` (every line of up to 8 things and every
landable count: the cut is a prefix, ends before a flight, equals `landable` at the fence ahead of it) and
`relay_tests::no_tell_lets_a_stale_byte_be_read` (a model: lines of up to 8 flights and tells, 9 card paces, with
and without the cut; at every tell every flight ahead is in title memory; same tells in the same order and the same
final memory). Line: "landings cut at the first tells behind their flights per frame".

**Shipped, no switch (same results):** a job's fetched-piece runs reuse one list (`relay_card_back`,
`relay_fetch_in_parts`); `relay_merge`, `card_merge` and `card_merge_relay` keep the "before" copy of a piece in
memory of the thread (`put_with_was`) instead of a new allocation per piece. The relay's fetch buffer and the
non-relay `CARD_WORK.fetched` were already kept and only grown. Not touched: `read_backs` maps a read-back buffer at
every landing (outside the functions this work may change).

**Shipped, off: `VICTORY_LAND_HELD=1|2`** (`shadow::land_held`, `host::relay_hold_landed`). What the bytes are for,
from the 7800X3D log of 2026-11-27 (*measured in game*): 441 of 756 landed pieces a frame go into private
(write-watched) title memory through the title's own address. The write watch counts those writes as the title's,
so the next refresh read each landed page back and compared it with its copy: 18.3 MB a frame "held already and
left", 0.1 MB "the title's too". The census's "our other threads first 98.8 %" is that read-back. Blocks landed beside
the guards (views) were already noted held (`VICTORY_LAND_TAKEN`, 318 of 319). With the switch the relay asks the
kernel about the landed private blocks itself, notes the answer as any asking is noted, compares the parts written
since the copy took each block, and notes a block that holds title memory's bytes as taken by that asking. The
refresh then reads nothing of it; a title write after the asking is taken as before. `=1` before the ranges are done
with (the fences wait for it: about 1.4 ms a frame *offline*, mostly kernel askings), `=2` after the title is told
(off the fences; a refresh that comes first reads back as before). Title memory is only read. Tests and the known
difference: `docs/changes-2026-11-27.md`, "Landed private blocks held by their copy". Line: "landed private blocks
per frame (VICTORY_LAND_HELD=N): ...".

What this does not do: it does not land less. The census cannot say whether the title reads landed bytes later: our
read-back touched every sampled private page first, and the census watches only the first touch. With
`VICTORY_LAND_HELD` on, that read-back is gone, so a census run with both on says what the title's own threads touch
first (T %), which is what landing on demand (2. below) needs to know before it is worth its risks.
**Designed, not shipped:**

1. *Fetch inside the flight* (the largest saving if the fetch wait is queueing). After the card's compare, a compute
   pass in the same flight copies the flagged 64 KiB pieces into a CPU-visible buffer (custom heap, write-back, L0,
   as UAV), compacted by an atomic counter. When the flight's fence is reached the pieces are there: no second
   list, no wait behind later flights. Needs the submit path (the compare is recorded there) and a worst-case
   buffer per flight; not testable offline for queue behaviour. Until then try `VICTORY_RELAY_COPY=1`.
2. *Copy back on demand.* Signal the fence before landing; guard the declared ranges `PAGE_NOACCESS`; the first touch
   lands that range in the fault handler. Holes: view memory is the guards' (`watch.rs`) and its handler and
   `writable()` would have to know a second kind of guard; the fault handler would wait for a fetch and take the
   merge locks on a title thread that may hold our console-call locks (deadlock); system calls on guarded memory
   fail instead of faulting; our own refreshes would fault too. Only private memory with no system-call use is
   safe, and the census says how much that is. Not safely testable offline: not shipped. Looked at again for
   2026-11-27 (with the 7800X3D log): of what lands, 58 % of pieces are private memory, 42 % views; the title is
   told after landing (`late_signal`), and the relay is 16 ms a frame idle and 18.8 ms landing (7.2 of them the fetch
   wait, 7.7 the merge). Fences sent before the landing would save up to that 18.8 ms of the fences' wait, but each
   hole above stands; part (1) of the plan, our own refreshes not reading back, is `VICTORY_LAND_HELD` above.
3. *Earlier fences* for a flight whose declared writes the CPU cannot read before a guard catches them: this is (2)'s
   guard; without it the only safe early fence is one behind flights with nothing to land, which segments give.
4. *Finer granularity.* The card's compare gives one bit per 64 KiB piece (`torn::flagged`); the merge already
   stores only changed dwords and skips unchanged 32-byte blocks. Finer pieces save only bus bytes and copy time
   (~1.6 ms for 40 MB) unless (1) is done. The landing line already holds the data: "pieces with a new byte" x
   64 KiB against "pages of 4 KiB with a new byte" and "MB in blocks of 32 bytes". If 4 KiB pages are under a
   quarter of the pieces' bytes, a 16-bit page mask per piece (a ballot of the 64 threads that hash a piece) is
   worth doing; it changes the compare shader and the submit path.

## 4. A/B plan for a game run

Same scene, warm cache, speed profile, Benchmark page, the frame log on. Keep the full 5 s lines: the frame line,
the relay line, "landing per frame", "landed per frame".

1. **Baseline.** From the relay line and the landing line, split "copying back" into pure fetch wait and work (1.).
2. **`VICTORY_BACK_CENSUS=1`** (measurement only). Read T, L, U, O and the private/view split. Run it alone.
3. **`VICTORY_RELAY_COPY=1`.** If the fetch wait is queueing, "copying back" and the title's fence waits ("a
   blocking fence wait" in the settles line) fall.
4. **`VICTORY_LAND_SEGMENTS=1`**, with and without 3. Watch "landings cut ..." (0 means the card is the pace and it
   cannot help), fence waits, "copying back" (it may rise: more fetches), fps and 1 % lows.
5. **`VICTORY_FETCH_PARTS=4`** with the best of 3 and 4: the merge beside the copy.

Decide (1) of section 3 by step 1 and 3; decide (2) by step 2 (it only pays if T is small and the private share is
large); decide (4) by the landed line's pieces against pages.
