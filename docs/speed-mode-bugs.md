# Speed set: stretched bodies, things through walls, rainbow skin

A tester sees bodies and textures fly through walls, stretch or jump, and skin turn rainbow-coloured with the tuned
speed set. `-Quality` takes it away (`scripts/play-nvidia.ps1`: `VICTORY_RELAY=1`, `VICTORY_CARD_STEPS=0`,
`VICTORY_TEXTURE_NO_WAIT=0`, about 20 -> 13 fps). This note says what each of the three gives up, which data then
arrives late, and two switches that wait for only that. Line numbers are those of the commit that added this note.
Nothing here is measured in game; the fps figures are estimates from the code and from `docs/findings.md`.

## What each fast setting gives up

**`VICTORY_RELAY=2` (against 1)** (`host.rs:2651` `relay_exact`). Flights land in the order they were sent, on
the relay thread, in both stages (`relay_land`, `host.rs:5635`); fences and bottom-of-pipe values go behind their
flight (`late_signal`, `host.rs:3684`), so the title's CPU never sees a fence before the shader bytes ahead of it are
in title memory. What stage 2 changes:
- a submission whose CPU steps read memory a flight declared written (a title buffer copy, a fill of upload memory,
  a texture: `blocked_by`, `host.rs:3329`) waits only until no flight left in the air declares it
  (`host.rs:6306`), not for everything;
- at 256 lists in the air it waits until there are fewer, not none (`host.rs:5997`);
- with card steps, title bytes under flights' writes wait only for the flights that declared them
  (`settle_stale`, `host.rs:6217`);
- it is the only stage card steps run in (`card_steps.rs:661`, `:464`).

Ordering given up: none for what `blocked_by` names. But a full wait used to land **every** flight, and so also
ordered reads nothing names. Stage 2 keeps later, unrelated flights in the air.

**`VICTORY_CARD_STEPS=2`** (`card_steps.rs:77`). A fill or a title buffer copy that reads memory flights may
write is done on the card in front of the submission, from the host copy the flights write, instead of on the CPU
after a wait. A copy's destination reaches title memory when its flight lands, not at submit. Ordering given up:
(a) code of ours that reads title memory away from a submission's steps finds the destination late
(`card_steps.rs:71` says so); (b) the wait it removes was the one that landed the skinning every frame (the
constant-ring fill and copy after the skinning, 25-30 ms of a 105 ms frame; `card_steps.rs` doc). With it gone,
nothing lands the skinning before the next submissions are prepared.

**`VICTORY_TEXTURE_NO_WAIT=1`** (`texture.rs:387`). A texture refresh that finds the runtime changing a mapping
(`watch.rs:2827` `MAPPING`, taken alone by `title_memory_remapping`, `watch.rs:2977`) does not wait for it. It
reads through the kernel (`refresh_unlocked`, `texture.rs:1421`); if a page is missing it is left for the next
refresh, or a never-filled staging buffer takes the pages there (`fill`, `texture.rs:1459`). Ordering given up: the
read can overlap a change of mapping. The runtime unmaps whole views and maps what stays page by page, so a read
that finds every page may still hold pages from before and after the change, or a page shown before the title's
data reached it. The read is taken for good: its hash is kept and the asking noted (`texture.rs:1303`), so later
refreshes find the texture "unwritten" (`texture.rs:1302`) until its hash turn, every 32nd frame in the tuned set.

## The symptoms

**Stretched or jumping bodies, things through walls.** The skinning dispatches write through a view of the title's
whole 174 MB heap. A host copy is the copy of the *innermost* buffer holding an address (`shadow.rs:5889` `find`).
A draw that binds skinned vertices through a buffer of its own inside the heap reads another host copy than the
one the skinning writes. That copy's refresh takes title memory as it is (`shadow.rs:4578` `update`). Nothing orders
it against the skinning's landing: `blocked_by` ignores refreshes (`host.rs:3386`), `stale_under_flights`
(`host.rs:3402`) only looks at copies whose own flights write them, and the card's writes are not told to other
copies (`shadow.rs:3876`). Refreshed before the landing, the draw gets last frame's vertices; during the landing,
a mix of two frames: stretched triangles, limbs a frame behind, which also matches "through walls" when the title's
collision uses current positions. With `RELAY=1`, or without card steps, the ring fill that follows the skinning
waits for it to land each frame and hides the race; card steps remove that wait. The same gap holds for our steps
that read title memory itself (`Step::Host`, the rectangle expansion, `draw.rs:3722`), but those are UI quads.

The title's own CPU reads (animation, physics read-backs) are ordered by fences and bottom-of-pipe values in both
stages, so they are not the likely cause.

**Rainbow skin.** The read across a change described above, kept until the hash turn: blocks of compressed colour
from another page decode as rainbow noise. Textures picked by index have no next refresh at all.

## The switches (off by default, read once)

**`VICTORY_CROSS_WAIT=1|2|count`** (`host.rs:3471`, `cross_settle` `host.rs:6246`, called at `host.rs:6317`
before the write-watch asking). The submission's refreshes, and steps of ours that read title memory, are checked
against what the flights in the air and being landed write through **another** host copy, or through none
(read-backs, textures copied back; `cross_writes`). Only if one meets does the submission wait, and only until no
flight left writes those bytes (relay: `relay_wait`; without it a full settle). Writes through the read's own copy
do not count (ordered on the card); bottom-of-pipe values do not count (`late_met` sees them). `2` counts only bytes
shaders landed in during the last 600 frames (`watch.rs:2068`): skinned buffers yes, static meshes in the heap no.
`count` waits for nothing and counts: the frame line "submissions that refresh title memory a flight writes through
another host copy". Card steps, stage 2 and their other savings stay.

**`VICTORY_TEXTURE_RECHECK=1`** (`texture.rs:431`). Around a read through the kernel the change count is read
(`watch.rs:2854`); if a change was under way or happened meanwhile (`met_change`) the texture's asking is forgotten,
so its next refresh reads and hashes it again, and its slots are looked at again (for textures picked by index).
Nothing waits. Frame line: "texture reads through the kernel that met a change of mapping, read again".

Tests (`cargo test -p victory-gpu --lib`): `host::cross_tests` (the wait is due exactly when a read follows a write
of another copy to the same bytes, not for the same copy, other bytes or touching ranges; on a line of synthetic
flights it ends after the last flight that writes the bytes, not before; against the plain pairwise rule on 2,000
fixed-seed flights), `texture::recheck_tests` (read again exactly when the count shows a change, only with the
switch). Off is the old code path: both are behind `if` on the switch, nothing else changed.

## Bisect on the Benchmark page

Use the speed profile and add the line(s) to the profile's raw `VICTORY_*` overrides; they go on top of the
script's values (launcher/README.md, "Overrides"). Same scene each run, a warm start, the frame log on. Watch the
bodies in a crowd scene for "stretched/jumping", a character close up for "rainbow skin".

| Run | Lines | Expected symptom change | Expected fps (speed ~20, Quality ~13) |
|---|---|---|---|
| A | none | both symptoms | baseline |
| a | `VICTORY_RELAY=1` | bodies fixed (also turns card steps off); rainbow stays | most of the 7 fps: -4 to -6 |
| b | `VICTORY_CARD_STEPS=0` | bodies fixed if the race above is it; rainbow stays | -2 to -4 (the ring wait comes back, but only for the skinning's flights) |
| c | `VICTORY_TEXTURE_NO_WAIT=0` | rainbow fixed; bodies stay | -0.5 to -2 in the world, worse while streaming (waits for remapping) |
| d | `VICTORY_CROSS_WAIT=2` and `VICTORY_TEXTURE_RECHECK=1` | both fixed if the mechanisms are right | -1 to -3: one wait per frame for the skinning's landing where it is read, plus re-reads |
| d1 | `VICTORY_CROSS_WAIT=count` | none (counts only) | none |
| d2 | `VICTORY_CROSS_WAIT=1` | bodies fixed | between d and b |

How to read it:
- **d1 first.** If the count is about 0 per frame, the cross-copy race does not happen in this scene and the bodies
  come from elsewhere; then (b) against (a) says whether it is card steps or stage 2's narrower waits.
- (b) fixes bodies and (d2) fixes them too: the race is it. (d) then shows whether the `2` narrowing keeps it fixed.
- (d) fixes bodies at the cost of (b): every skinned draw waits for the skinning, as before card steps. The next
  step is to refresh the inner copy from the skinning's copy **on the card**, in a card step behind the flight
  (as card steps copy a buffer), so nothing waits.
- (c) fixes rainbow but (d) with `RECHECK` does not: the bad bytes are not torn reads but pages shown before the
  title's data came; then look at `fill` (`texture.rs:1459`).
- Compare the log lines "waiting for landings: title memory read" (time) and the two new frame lines between runs.
