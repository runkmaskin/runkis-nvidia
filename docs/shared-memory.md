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

## Write-combined title memory in the card's memory (`VICTORY_WC_VRAM=1`, off)

The title tells us which memory its CPU only writes: protection bit 0x400, write-combined, in `XMemVirtualAlloc`.
On Series S its 1,277 MiB graphics pool is such memory (protection 0x2404, kind commit, flags 0x283; at an address
of our choice in [4 GiB, 2^40)). In the 7800X3D log of 2026-11-27 two shader-written copies in it had 14.6 and 4.9
MB fetched back a frame and nothing sent up. Copies in the title's second write-combined area (0x42404/0x2404 with
kind 0x42000) were fetched back too, but that area is a placeholder for physical-page views (port 1's kind of
memory), not one allocation. It is left alone here.

**What the switch does** (`crates/victory-gpu/src/wcvram.rs`, hooks in `xmem.rs`, `shadow.rs`, `host.rs`,
`host/place.rs`, `draw.rs`, `queue.rs`):
- That pool (16 MiB or more, write-combined, committed, no address asked for) becomes one
  `D3D12_HEAP_TYPE_GPU_UPLOAD` heap, with a buffer over all of it mapped for good. The mapping is the title's
  allocation: no `VirtualAlloc`, no write watch, zeroed as committed memory is.
- Title buffers and windows in it are buffers placed in that heap (the `Shadow` in-place form of `VICTORY_INPLACE`).
  There is no host copy, no refresh, no compare and no landing for them.
- What shaders write there is declared as for any copy, so that our CPU steps that read it wait for the card as
  they waited for the landing. The landing then finds nothing to merge.
- A constant buffer of a table in the pool on 256 bytes is a constant buffer view of the pool itself (no fill).
- A title buffer copy into the pool is a copy on the card, at its place in the list (`place.rs`,
  `copy_into_card`). Title memory there is not written or read on the CPU.
- Each title `ExecuteCommandLists` calls `FlushProcessWriteBuffers` (ordering, below).

**The address.** Direct3D maps the heap where the system finds room. Measured: at 0x1be_b046_0000 to
0x2df_a0ce_0000, above 2^40, where translated shaders cannot reach (`upload.rs`). The system takes the first free
stretch big enough, from a place of its own upwards and then from the bottom. So `wcvram::steered` reserves, for
the moment the heap is made, every free stretch that could hold it plus everything around the target, except the
place `xmem` would have used. The mapping then lands there every time (tests below; in game at 0x948c00000 in
1.9 s). Allocations of the pool's size or more by other threads fail during that moment. A 64 KiB write-combined
allocation of the title's went elsewhere; small ones are left alone now.

**Facts on this PC** (7800X3D, RTX 4070 SUPER, *offline*, `bench_card_memory_the_title_writes`, release):

| | write-back (today) | card memory the CPU maps |
|---|---|---|
| CPU memcpy write 64 KiB / 1 MiB / 16 MiB / 64 MiB | 60 / 39 / 22 / 14 GB/s | 20 / 15 / 10 / 14 GB/s |
| CPU 8-byte stores every 64 bytes over 16 MiB | 0.18 ms | 2.8 ms |
| CPU memcpy read 4 MiB | 23 GB/s | **0.024 GB/s** |
| CPU dependent load | 26 ns | **875 ns** |
| `FlushProcessWriteBuffers` | | 0.3 us (test process); 6.7-7.9 us in game, 17 a frame |

`GPUUploadHeapSupported` is true. The mapping is private memory with protection 0x404 (read-write,
write-combine). `GetWriteWatch` refuses it. `VirtualProtect` to read-only worked on a 1 GiB heap and was refused
(error 87) on a 64 MiB one, so the guards of `watch.rs` cannot be relied on there.

**Why `VICTORY_WC_NO_BACK` showed black flashes and a quarter fewer draws** (that switch is not in this tree; the
reasoning is from the code and the traffic log). The pool's copies are written by shaders and fetched back every
frame (14.6 + 4.9 MB). Not landing them left title memory without the card's results. The title's CPU reads them
(its visibility and culling results: fewer draws). So do our own CPU steps and other host copies of the same
bytes, which then made stale textures and buffers (flashes). Here the card's writes are the title's memory, so
both get the right bytes with no landing, but every CPU read crosses the bus uncached.

**Order.** A card write is visible to the CPU once the flight has run. The title learns of it from its fences,
which are set after the flight's landing as before. A CPU write must leave the CPU's write-combining buffers before
the card reads it. `sfence`, `mfence`, locked instructions, serializing instructions, uncached accesses and
interrupts empty them (Intel SDM vol. 3 11.3.1, AMD APM vol. 2 7.7). A plain store does not, and that includes a
release store on x86. A title thread that wrote and then told the submitting thread with a plain store could still
hold the bytes. `FlushProcessWriteBuffers` at each title submission interrupts every processor that runs a thread
of ours, which empties their buffers. Threads not running emptied theirs when they were switched out. Our own
writes are emptied by the runtime's locked operations in `ExecuteCommandLists`.
`model_the_card_sees_every_store_once_the_buffers_are_flushed` shows the race without the flush, and none with it
or with a locked telling.

**Measured in game** (768p Series S, speed set of `play-nvidia.ps1`, scripted Enter from 20 to 140 s, seconds
150-300; one run each):

| Run | fps | 1 % low | preparing ms | copying back ms | fetched MB/frame | peak private |
|---|---|---|---|---|---|---|
| off (23:26) | 23.5 | 11.5 | 22.1 | 16.8 | 41.0 | 25.1 GB |
| off (00:51) | 21.0 | 11.0 | 21.8 | 17.3 | 40.3 | 25.2 GB |
| `VICTORY_WC_NO_BACK=1`, `_MIN_KB=1024` (00:41) | 25.4 | 13.4 | 18.8 | **5.8** | 5.8 | 25.7 GB |
| `VICTORY_WC_VRAM=1`, first version (23:06) | **3.1** | 1.8 | 353 | 13.3 | 21.7 | 22.2 GB |
| `VICTORY_WC_VRAM=1`, constants and copies on the card (00:46) | **5.3** | 3.2 | 144 | 6.4 | 18.0 | 22.6 GB |

In the first version 3,777 title buffers and 68 windows (559 MB) were in the pool. Fetches fell by about half, and
the picture in the captures was right. But our CPU still read the pool every frame: 989 fills of constant buffers
(1.9 MB), 28 buffer copies (131 KB) and 65 textures staged (289 KB), at 24 MB/s. That and the uncached hashing of
textures that are not write-watched made preparing 353 ms. The constant buffers on 256 bytes and the buffer copies
into the pool have been moved to the card since; see `docs/changes-2026-11-27.md` for the run after that.

The second version left 630 fills of 111 KB and 3 copies of 2 KB a frame on the CPU. Only 325 of about 950
constant buffers a frame lie on 256 bytes; the rest are still copied. Textures were 64 staged of 280 KB a frame.
The frame was still 160-196 ms. Its own lines say where the time goes:
- "texture refreshes per frame: 251 ms, 227 ms asking the write watch and the guards". The write watch refuses
  the pool, and the guards cannot be relied on there, so every check of a texture in the pool asks again and
  fails, then hashes the texture through the bus.
- "copies 85 ms".
- "41 ms on the GPU" against 8-14 off. Not explained yet: the card should read this memory at its own speed.

**`VICTORY_WC_NO_BACK`** is the measured part. It is ported from trustno1-coder/VIctory-NVIDIA 50c15a1 and
2f81d0c (GPL-3.0), and it lives in `wcvram.rs` with its check in `Shadow::gpu_writes`. Shader writes into card
copies of 1 MiB or more in write-combined memory stay on the card. "Copying back" fell from 17 to 5.8 ms a frame
and fetches from 40 to 5.8 MB, at +2 to +4 fps against the two off runs. Its known risks: the title or another host
copy reading such bytes reads old ones, and a page the title writes goes up whole over shader results. The 1 MiB
floor is the fork's answer to its near-black cutscenes. In the captured frames 4000-4007 of this run one
character's face was rainbow-coloured, the "rainbow skin" of `docs/speed-mode-bugs.md`. The off runs' captures
show other shots of the same scene, so the switch can be neither blamed nor cleared for it from these frames. The
other faces and the picture were right, with no flicker between consecutive frames (luma changes under 3.3 of
255 between frames, against camera cuts of 9.9 in the off run). One run each: repeat before turning it on.

**What is still read on the CPU** (counted on the frame-rate line "write-combined title memory in the card's
memory"): constant buffers not on 256 bytes, copies out of the pool into memory elsewhere, and textures in the
pool. `texture.rs` detiles and hashes title memory on the CPU. A texture there would need its bytes fetched by the
card into the PC's memory first, or a detile on the card. Not done.