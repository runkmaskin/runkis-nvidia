# Shared memory: how title memory reaches the card, and whether zero-copy pays

The console has one memory; a PC has two. Every byte a shader reads of title memory is either read over
the bus from the PC's memory or copied into the card's memory first. This note maps today's paths, measures
the alternatives on an RTX 4070 SUPER (Ryzen 7 7800X3D, PCIe 4.0, ReBAR on) and says which fits which use.
All figures are *offline* (`zerocopy.rs` benchmarks), none from a game run.

## The map (file:line at commit 40d21f3 plus this change)

**Host copies** (`shadow.rs`). A `Shadow` is a host buffer for a title buffer or a window of title memory.
- Title buffers: `lookup` (shadow.rs:5924) makes the copy on first use. The CPU's side is always
  `Host::shared_buffer` (host.rs:10074): custom heap, write-back pages, pool L0, **mapped for good**. Small
  buffers are read by the card from there, over the bus.
- Big title buffers (`cards()`, shadow.rs:164; size limits by VRAM budget, `card_limits`): the CPU's side
  above plus a DEFAULT-heap buffer (`Host::uav_buffer`, host.rs:10217) that shaders see. Changed ranges go
  up before each submission (`card_dirty` shadow.rs:627 -> `Host::card_before` host.rs:4561). **Copied twice
  by design**: title -> CPU side (CPU) -> card side (card). The CPU side is the reference for the dword
  compares and for merging shader writes, so it cannot go.
- Windows outside buffers (`loose`, shadow.rs:6237): 4 MiB (or `VICTORY_WINDOW_KB`) of the PC's memory each,
  read over the bus. Options, all off: `VICTORY_WINDOWS_BAR_MB` (card memory the CPU maps, GPU_UPLOAD,
  host.rs:10158, shadow.rs:513), `VICTORY_CARD_WINDOWS_MB` (two sides), `VICTORY_INPLACE=1` (zero-copy).
- Refreshes: `update` (shadow.rs:4578) -> `take_all`/`take` (shadow.rs:4920) / `copy` (4978) through
  `host::copy_mapped` (the safe copy, `safecopy.rs:398`). Snapshots for CPU compares: `take_snapshots`
  (shadow.rs:1958), pooled (`pooled`/`give_back`).
- Counters and lines: `memory by subsystem: windows=… gpu_used=…` (`memory_counts`, shadow.rs:503); the fps
  line's `copies X ms` (host.rs:9643); `send_stats` (SEND: bytes looked at / unchanged / sent / runs);
  `card_stats`; "no card copy for a title buffer…" (shadow.rs:403); "shader writes looked for on the CPU per
  frame…" (shadow.rs:2121); "title memory in place: …" (inplace.rs `say`).

**Uploads.** Texture staging is an UPLOAD heap (write-combined), **mapped for good**, cut from 64 MiB blocks
(`pool::staging`, pool.rs:356; `Host::upload_buffer` host.rs:9239). `texture.rs` detiles title memory straight
into it (`Staging::detile`, texture.rs:1439; one CPU pass, written only), then the card copies it into the
texture. The kernel-read path while mappings change (`refresh_unlocked`, texture.rs:1373) reads into a scratch
first: two copies, needed because the pages may go. `upload.rs`: memory the title writes and shaders read by
address, arenas below 2^40 that are title buffers to the copies.

**Write watch and guards** (`watch.rs`). The kernel's write watch for private memory (`ask_once` watch.rs:547,
`writes` watch.rs:678), read-only guards and our fault handler for physical-page views (`install`
watch.rs:3160). It decides which 64 KiB pages a refresh copies; nothing is copied without it.

**Copy-back and landing** (`land.rs`, `shadow.rs`). Card sides are compared on the card, changed pieces fetched
and merged into title memory once (`safecopy::merge`, land.rs; `Shadow::land_back` shadow.rs:5427,
`card_merge` 5215). One-sided copies are compared with their snapshot on the CPU (`copy_back_with` 3998).
Lines: "landing per frame…" (land.rs:171), "landed per frame…" (land.rs:213).

**Title memory by address** (`memory.rs`): pages outside buffers go into arenas, which are `by_address`
copies (shadow.rs:6186): a CPU side plus a card side (or a second PC-memory side past
`VICTORY_MEMORY_CARD_MB`); only changed dwords go up. Line: "title memory by address: … pages … arenas"
(memory.rs:765).

**Zero-copy exists already**: `VICTORY_INPLACE=1` (windows in private memory) and `=2` (title buffers too),
`inplace.rs`: `OpenExistingHeapFromAddress1` on the title's pages, one placed buffer over them, opened once
and held by the `Shadow` (`place`). View memory (the page pool) is refused.

## The benchmark

`cargo test --release -p victory-gpu --lib -- --ignored --nocapture bench_shared_memory` (zerocopy.rs). Title-
like memory (private, committed, write-watched), every byte changed each frame; the CPU fills the way's side
with the safe copy as a refresh does; the card copies if the way has two sides, then a compute shader reads
every 16 bytes once or 16 times. Medians of 9 frames, microseconds, **CPU fill + card time** (card time =
copy + reads, from timestamps). Custom WC L0 measured the same as UPLOAD and is left out.

| size x reads | shared WB (today) | WB + copy to VRAM (today, big) | UPLOAD (WC) | GPU_UPLOAD (ReBAR) | in place, opened once | in place, opened each frame |
|---|---|---|---|---|---|---|
| 64 KiB x1 | 1.6 + 6.1 | 1.3 + 8.1 | 2.0 + 6.1 | 2.3 + 2.0 | 0 + 6.1 | 227 + 6.1 |
| 64 KiB x16 | 1.4 + 6.1 | 1.8 + 10.1 | 1.9 + 6.1 | 2.3 + 4.1 | 0 + 21-30 | 190 + 21 |
| 1 MiB x1 | 22 + 42 | 20 + 46 | 33 + 42 | 46 + 3.1 | 0 + 42 | 323 + 42 |
| 1 MiB x16 | 21 + 49 | 20 + 55 | 33 + 49 | 46 + 11 | 0 + **573** | 335 + 576 |
| 4 MiB x1 | 135 + 159 | 133 + 164 | 132 + 159 | 184 + 4.1 | 0 + 159 | 871 + 159 |
| 4 MiB x16 | 132 + 183 | 132 + 193 | 132 + 184 | 184 + 33 | 0 + **2,446** | 956 + 2,447 |
| 16 MiB x1 | 536 + 626 | 527 + 650 | 529 + 626 | 744 + 9.1 | 0 + 626 | 2,641 + 626 |
| 16 MiB x16 | 531 + 715 | 527 + 744 | 532 + 715 | 743 + 103 | 0 + **10,984** | 3,140 + 11,081 |
| 64 MiB x1 | 2,393 + 2,682 | 2,331 + 2,806 | 2,338 + 2,648 | 2,985 + 108 | 0 + 2,496 | 12,783 + 2,497 |
| 64 MiB x16 | 2,445 + 44,320 | 2,348 + 4,747 | 2,500 + 43,934 | 2,997 + 2,380 | 0 + 44,954 | 14,888 + 43,791 |

What it shows:
- The bus gives 25-27 GB/s whatever the heap; VRAM reads 450-2,600 GB/s.
- **The card caches the PC's memory in its L2 for heaps we make, but not for a heap opened on an address.** Read
  16 times, a shared copy of up to 16 MiB costs little more than read once (it stays in the 48 MB L2); in place
  costs 16 times (1 MiB: 573 us against 49). Past the L2 (64 MiB) every PC-memory way pays the bus each read.
  Why the opened heap is not cached is not known (a write-back heap made shared across adapters, to tell, is
  refused by the driver).
- Opening a heap on an address costs 0.2-15 ms (it locks the pages): never per frame.
- ReBAR: the card reads at VRAM speed with no copy of its own; the CPU's writes through the bus cost 25-40 %
  more than a copy into the PC's memory (184 against 135 us for 4 MiB). The CPU must never read it.
- In place saves the whole CPU fill (and the refresh, and its write-watch asking for that memory).

## The decision

The layer is CPU-bound (about 48 ms of a 55 ms frame on the submitting thread), so CPU time is what counts first,
card time second. Per use:

| Use | Best | Why |
|---|---|---|
| Big title buffers, read many times, written by shaders | today (VRAM card side) | in place would read 16x slower uncached; ReBAR cannot keep the CPU reference |
| Textures | today (UPLOAD staging -> texture) | read many times; the staging is written only |
| Windows: read once a frame, CPU-written, no shader writes | `VICTORY_INPLACE=1` if they are read about once; else today | saves the fill; same card time read once, many times slower read again |
| Same, if card time matters | `VICTORY_WINDOWS_BAR_MB` | card reads 20-80x faster; CPU fill 25-40 % dearer |
| Arenas by address (rays walk them) | today (card side) | read with every ray step |
| "The card's own buffers where fills copy them" (submit-thread.md #5) | not done | changes memory order; needs the game |

**No new zero-copy path was added.** Both candidates the benchmark supports already exist behind default-off
switches (`VICTORY_INPLACE=1`, `VICTORY_WINDOWS_BAR_MB`), and neither is a clear win without knowing how often
the card reads a window per frame: in place wins the CPU fill and loses badly on repeated reads; ReBAR wins the
card and costs CPU. Open risks of in place, for anyone who turns it on: while a heap stands the title cannot
decommit, free or protect those pages (calls fail with error 87; `title_memory_freeing` is the hook); the heap
is held by its `Shadow` (`place`), and `inplace.rs` says it goes when the last list and slot that hold it do
(read in the code, not proven by a test here). `=2` must not be combined with
`VICTORY_LATE_WAIT`. The equivalence test shows every way, in place included, hands the shader the title's
current bytes.

Done here instead (no switch needed: same results, tested):
- `Shadow::take` of a page the copy never held (two-sided copies with dword sends) read it into a page of its
  own and then copied that page whole into the CPU's side. It now goes straight in: one 64 KiB copy fewer a page,
  0.64 -> 0.31 ms per 16 MiB (`bench_a_page_taken_whole`; equivalence:
  `a_page_taken_whole_goes_in_as_through_a_page_read_first`, with a missing page that must read as zeros).
- `SentRuns::note_joined` added to its vector instead of building a new one of both
  (`sends_noted_again_are_joined_as_before`, fixed seeds against the old concat).

## What a game run must show

A/B on the Benchmark page, same scene, warm cache:
1. `VICTORY_INPLACE=1` on and off. Watch: the fps line's `copies X ms` (should fall) and `{gpu} on the GPU`
   (must not rise much: that is the uncached re-read); "title memory in place: N windows M MB … turned down …";
   `memory by subsystem: windows=` (falls; the title's pages are locked instead); no "error 87" or lost writes
   in the log; no missing UI or vertices.
2. `VICTORY_WINDOWS_BAR_MB=1024` on and off: `copies X ms` (may rise by a quarter), `on the GPU` (should fall),
   `gpu_used` (rises by the windows' bytes).
3. Either way, compare 1 % lows: in place pins memory and ReBAR takes VRAM from textures.
