# NVIDIA DLSS for the title

Goal: DLSS Super Resolution (the transformer models of DLSS 4 / 4.5) in place of the title's own temporal upscaler,
then DLSS Frame Generation. This file says what is done, what was found about the title's upscaler, and the plan for
the rest. Nothing of NVIDIA's and nothing of the game is in the repository.

## Setup

1. `scripts\get-dlss.ps1` downloads NVIDIA's public Streamline SDK release 2.14.16 from GitHub (about 280 MB, once),
   checks its SHA-256 and keeps only the signed runtime files in `%LOCALAPPDATA%\victory\dlss` (`sl.interposer.dll`,
   `sl.common.dll`, `sl.dlss.dll`, `nvngx_dlss.dll` 310.9.16, and for later `sl.dlss_g.dll`, `nvngx_dlssg.dll`,
   `sl.reflex.dll`, `sl.pcl.dll`), with NVIDIA's licence texts. Running it means accepting those licences.
   `-To <dir>` puts them elsewhere; then set `VICTORY_DLSS_DIR=<dir>`. The script refuses a folder inside the
   repository, and `.gitignore` already keeps `*.dll` and `*.zip` out.
2. `VICTORY_DLSS=1` (off by default). The layer then loads `sl.interposer.dll` from that folder, only if its
   Authenticode signature is valid (`WinVerifyTrust`), starts Streamline and hands it the host device right after
   the device is made (`host.rs`, `create`). The log says `DLSS: Streamline started; DLSS Super Resolution is
   supported on this card`, or why not (`DLSS not available: ...`, with Streamline's result named). Streamline's
   own log and NGX's are in `%LOCALAPPDATA%\victory\dlss-logs\sl.log`.
3. `VICTORY_DLSS_PRESET=J|K|L|M` picks the model (K is DLSS 4's transformer, L and M DLSS 4.5's). Unset: NVIDIA's
   default for the mode, which for DLSS 310.9 is a transformer model as well (M for Performance, K for Quality).

Over-the-air updates of Streamline and NGX are off (`eAllowOTA` is not set): nothing is downloaded while the title
runs. NGX keeps its model cache in `C:\ProgramData\NVIDIA\NGX\models`, as for any game.

## How Streamline is attached (`crates/victory-gpu/src/dlss.rs`)

Streamline is used with **manual hooking** (`eUseManualHooking`): the host device is made as before and given with
`slSetD3DDevice`. No D3D12 or DXGI call goes through Streamline's proxies, so nothing else in the layer changes.
The structures are the layer's own declarations of Streamline's C ABI (its headers are MIT-licensed); a test checks
every size and offset against the headers' layout (`structures_have_streamlines_layout`).

**Is `slSetD3DDevice` on the existing device enough?** For DLSS Super Resolution, yes: proved by
`dlss::tests::gpu_dlss_upscales_a_synthetic_frame` (below). Super Resolution is one `slEvaluateFeature` recorded
into a command list with the four textures tagged locally (input colour, depth, motion vectors at render size;
output colour at output size, with the D3D12 state each is in, which Streamline cannot track under manual hooking).
For **Frame Generation it is not enough**: under manual hooking Streamline must see the swap chain's creation and
every present (`IDXGIFactory::CreateSwapChainForHwnd`, `IDXGISwapChain::Present/GetBuffer/ResizeBuffers/
GetCurrentBackBufferIndex`) and the queue's creation (`ID3D12Device::CreateCommandQueue`): the factory and the
device used for those must be upgraded to Streamline's proxies with `slUpgradeInterface` (see steps 3-4).

After an evaluation the command list's descriptor heaps, compute root signature, root arguments and pipeline are
DLSS's; the layer must set its own again (`eDisableCLStateTracking`, the default). `State::replay` in `draw.rs`
already forgets all of them for the next draw or dispatch.

## Proof: DLSS Super Resolution on a synthetic frame

`cargo test -p victory-gpu dlss::tests::gpu -- --nocapture` (skipped with a message when the files are absent or
the card is not an NVIDIA RTX): a 960x540 RGBA16F frame of two flat halves, depth one plane, no motion, upscaled to
1920x1080 eight times with Halton jitter, mode Performance. Measured on the RTX 4070 SUPER (driver of 2026-11):

```
DLSS Performance at 1920x1080: render (960, 540), from (960, 540) to (1920, 1080)
8 DLSS frames recorded and run in 372.7706ms (the first makes DLSS's own resources)
largest difference from the flat colours: 0.0017
```

Streamline's log: `sl.dlss` 2.14.16 PRODUCTION loaded, NGX found a model id for the custom engine ("victory"), the
feature `dlss` created from `nvngx_dlss.dll`.

## Step 1: the title's upscaler at run time

How it was found, without anything of the game in the repository: `victory shader-info --verbose` on the game's
shader archive lists every program by name (kept outside the repository); the temporal ones are
`TAA2:CS_TAA2_Resolve` (hash `df11dcd012cd7382`), `TAA2U:CS_TAA2_Resolve` (`22f4971293b8fdcc`, the same source
compiled as an upscaler, twice the code), `PS_TAA2_SpatialUpscale[WithDithering]`, `TemporalAA:CS_ApplyTemporalAA`
and the `TAA_Utilities` helpers. Then two runs with `VICTORY_TAAU_REPORT` (diagnostics build, Series S, 540p,
`VICTORY_OWN_UPSCALER=1`, render scale 0.5, the scripted run into play), logs and captures kept in the scratch
folder and deleted afterwards.

**What the title runs with the own upscaler at half scale** (every frame, in this order):

1. `CS_ApplyCombinedDeferred` writes the lit scene, 960x540 RGBA16F (HDR).
2. **`TAA2:CS_TAA2_Resolve`** (`df11dcd012cd7382`), compute, 120x68 groups of 8x8 = 960x544 threads: temporal
   anti-aliasing **at render size**. Its bindings (root parameter 1, place in its table in dwords):

   | Place | View | What | Size, host format |
   |---|---|---|---|
   | 32 (and root 0 place 88) | SRV | depth | 960x540, R32G8X24 typeless (D32S8) |
   | 56 | SRV | colour: the lit scene | 960x540, R16G16B16A16 float |
   | 72 | SRV | exposure (1 texel) | 1x1, R32G32B32A32 float |
   | 80 | SRV | motion vectors | 960x540, R16G16 float |
   | 24, 88, 104 | SRV | masks | 960x540, R8 unorm |
   | 112 | SRV | history (last frame's) | 960x540, RGBA16F; swaps with place 136 each frame |
   | 136 | UAV | history (this frame's) | 960x540, RGBA16F |
   | 144 | UAV | mask | 960x540, R8 unorm |
   | **160** | **UAV** | **the result**, read by the post-processing draw | 960x540, RGBA16F |
   | 168 | CBV | the frame's camera constants (480 bytes) | |
   | 176 | CBV | sizes: render 960x540 and its reciprocal, output 1920x1080 and its reciprocal | |

3. Post-processing at render size (a whole-screen draw, pixel shader `6318f669e4f17d65`, reads place 160's
   texture) writes the finished picture, 960x540 R10G10B10A2.
4. **`PS_TAA2_SpatialUpscaleWithDithering`** (pixel `4f4ec7b16673362f`), a whole-screen draw, scales it to the
   1920x1080 RGBA8 game plane: a spatial upscale of the finished picture, not a temporal one.
5. At the present, `compose.rs` lays the interface plane (3840x2160, a separate plane of the Series S) over the
   game plane. **The interface never passes through TAA or the upscale**, which is what Frame Generation wants.

`TAA2U:CS_TAA2_Resolve`, the real temporal upscaler, was never dispatched in these runs (it was named in the report's list): with the profile
that `VICTORY_OWN_UPSCALER` forces (`mov eax, 6`, `hooks.rs`) the title runs TAA plus a spatial upscale.

**The frame's constant buffer** (place 168, 120 words): words 0-15 the view matrix, 16-31 the projection, 32-47
the view's inverse rotation, 48-50 the camera position, 56-87 a second view and projection, 88-119 view-projection
matrices. The projection is **reversed-Z with an infinite far plane** (word 30 = 0.05 the near plane, word 26
about 0: depth is near/distance, 1 at the near plane, 0 at the sky). **The jitter is words 24 and 25**, the
projection's third row: per frame, in clip units, a repeating Halton-like sequence (0, 0), (-0.000511, 0.001593),
(-0.001021, -0.000517), (0.000551, 0.001076), (-0.000470, 0.000559), (-0.000981, -0.001552), (0.000592, 0.000041),
... seen identically in runs a minute apart; at 960x540 that is up to half a pixel. With row vectors the clip
offset is -x, so a pixel's offset is (-x times 480, y times 270) (clip y up, pixels down); the sign is the one
thing to check by eye (`VICTORY_DLSS_JITTER_SCALE=<x>,<y>` changes it without a rebuild).

**Motion vectors** are R16G16 float at render size; their scale (pixels or a fraction of the screen) and sign
come from the frame dump's channel values (`target channel values ... format=34`).

In the captured frame of the run (a still shot of the opening) the motion texture held exactly 0 everywhere, so the scale is not known yet. The map assumes a fraction of the screen (`mvecScale` 1, 1); `VICTORY_DLSS_MOTION_SCALE=<x>,<y>` (for pixels: `0.0010417,0.0018519` at 960x540, or with a minus sign for the other direction) sets it without a rebuild. A capture while the camera turns (F9 with `VICTORY_CAPTURE` and `VICTORY_DUMP_TARGETS=1`, or main's `VICTORY_DATA_BREAK` on the writer of the motion target) settles it.

The pass is recognised at run time by its shader hash (the hash the log uses for every shader); the generic shape
rule (`taau::upscale_shape`) finds upscalers that write at output size and rightly did not take this TAA for one.

## Step 2 in the live frame

`VICTORY_DLSS=1` also puts DLSS into the frame (`draw.rs`, `State::dlss_after`, after the dispatch of the pass
`dlss::PASS` names): the TAA pass runs as before, then DLSS is recorded from its colour (place 56), depth (32) and
motion vectors (80) into its result (place 160), with the jitter from words 24-25 of place 168. The textures are
moved to the states DLSS reads and writes through the list's own state tracking (`list::ensure_state`), a UAV
barrier makes the title's result written first, and afterwards the layer forgets its pipeline, root signatures,
heaps and root arguments so the next draw or dispatch sets them again. Each evaluation gets a frame number of its
own: the title records the pass twice in some frames and ahead of the layer's frame counter, and Streamline
refuses a second set of constants for a number it has seen ("Setting different 'common' constants multiple times
within the same frame is NOT allowed!", run 3; fixed for run 4).

Because the title's TAA runs at render size, DLSS runs there as **DLAA** (960x540 in and out, the mode follows the
ratio: `dlss::mode_for`); the title's post-processing and its spatial upscale follow unchanged. Measured in game
(run 4, 7800X3D / RTX 4070 SUPER, 540p, diagnostics build with the report and captures on, so not an fps figure):
`DLSS: ... render=(960, 540) out=(960, 540) mode=Dlaa range=Ok([(960, 540), (950, 535), (960, 540)])`, every
evaluation from then on succeeded (one Streamline message, below), and the frames saved in play (the prison, ten
seconds and more after DLSS started) show a whole, normal picture. Image quality against the title's TAA was not
judged; the jitter sign and the motion scale are the two things to check by eye.

Streamline logs once: "presentCommon() was not observed ... internal bookkeeping and garbage collection will not
run". Under manual hooking its per-frame bookkeeping runs in the present of a swap chain it knows. For long play
with DLSS the swap chain must be upgraded to Streamline's proxy (`slUpgradeInterface` on the factory before
`CreateSwapChainForHwnd`, and `Present` through the proxy), which is also the first step of Frame Generation.

**To get real upscaling** (render 540p, DLSS to 1080p), one of:
1. Make the title take its real TAAU (`TAA2U:CS_TAA2_Resolve`, `22f4971293b8fdcc`): its post-processing then runs
   at output size and DLSS takes that pass's place 1:1 (the bindings are the same layout as `TAA2`'s, read from
   the shader offline). Needs the profile value the title uses for it (`hooks.rs` forces 6), found in the
   executable; per game build.
2. Or move DLSS to the end: replace the TAA's result with its jittered input (TAA off), and evaluate DLSS at the
   spatial upscale draw (`4f4ec7b16673362f`) from the post-processed 960x540 picture (LDR, `colorBuffersHDR`
   false), depth, motion and the jitter, into a 1920x1080 texture of ours copied over the game plane (the plane is
   a render target without unordered access). Post-processing then works on the un-anti-aliased picture.
## Steps 3 and 4: Frame Generation and Reflex (plan)

What Frame Generation (DLSS-G) needs, and where each piece lives in this layer:

| Needs | Where it is / would go |
|---|---|
| Streamline sees the swap chain made and every present | `host.rs`, `Host::presenter`: `CreateDXGIFactory2` then `CreateSwapChainForHwnd(&self.queue, ...)`. With FG: `slUpgradeInterface(&factory)` first, so the swap chain is Streamline's proxy; `present_over` then calls `Present`, `GetBuffer`, `GetCurrentBackBufferIndex` and `ResizeBuffers` on the proxy. Everything else (the copy lists, `upscale.rs`, `compose.rs`) keeps the native objects (`slGetNativeInterface`). |
| The queue that presents made through Streamline (`eID3D12Device_CreateCommandQueue` is a mandatory hook) | `host.rs`, `create`: `device.CreateCommandQueue` for `Host::queue`. With FG: a proxy of the device (`slUpgradeInterface(&device)`) makes that one queue; the device itself stays native. |
| Features loaded: `kFeatureDLSS_G` (1000), `kFeatureReflex` (3), `kFeaturePCL` (4) | `dlss.rs`, `start`: the `features` list. `scripts\get-dlss.ps1` already fetches `sl.dlss_g.dll`, `nvngx_dlssg.dll`, `sl.reflex.dll`, `sl.pcl.dll`. |
| HUD-less colour (`kBufferTypeHUDLessColor`) at back-buffer size | `host.rs`, `present_over`: `texture` before `compose::record` is the game plane without the interface; the composed texture (`laid`) is what reaches the back buffer. Tag `texture` as HUD-less, valid until present. |
| The interface (`kBufferTypeUIColorAndAlpha`, optional) | `compose.rs`: the Series S interface plane `ui` (3840x2160, straight alpha) is separate already; tagging it lets DLSS-G keep the interface out of the interpolation, which is the classic cause of "broken letters" in generated frames. |
| Depth and motion vectors (render size, with their extent) | The TAA pass's own inputs (step 1 map): tag them at that dispatch with `eValidUntilPresent`, in the state they are left in, or copy them into textures of ours there (the title reuses its targets within the frame). |
| Camera matrices: `cameraViewToClip`, `clipToCameraView`, `clipToPrevClip`, `prevClipToClip`, camera position and axes, near, far, FOV | The TAA pass reprojects with the current and the previous view-projection: they are in its constant buffers (step 1 lists their words every 600 frames). Without jitter, row-major. `sl_matrix_helpers.h` shows how the previous-clip matrices are made from view and projection. |
| `slDLSSGSetOptions` (on, frames to generate, flags) | Once at start and when the window is resized (`presenter`). |
| Reflex / PCL markers per frame (`slPCLSetMarker`): simulation start/end, render submit start/end, present start/end; `slReflexSleep` | Simulation start: the title's frame-origin wait ends (`frame.rs`, the tick handed out: `wait`). Simulation end: the title's present call reaches the layer (`queue.rs`, the plane present). Render submit start/end: around the frame's submissions (`queue.rs` execute, the host `execute`). Present start/end: around `Present` in `present_over` (`host.rs`). `slReflexSleep` at the origin wait instead of (or before) the layer's own tick. |
| Frame token per frame | `slGetNewFrameToken` with `frame::current()` at the origin wait; every tag, constant and marker of that frame uses it. |

**The 60 fps cap and CPU-bound frames.** `VICTORY_FRAME_INTERVAL_US=16667` lets the title start a frame every
16.7 ms at most; the layer is CPU-bound well below that (27-35 fps measured at 540p on the 7800X3D). DLSS-G then
presents one generated frame between two real ones: about 54-70 presented fps from 27-35 real, with the latency
of the real rate plus about one real frame (DLSS-G holds the newest real frame back to interpolate). NVIDIA's own
guidance is a base of 40-60 fps for good results; at 27 fps generated frames show more artefacts in fast motion.
With FG on, the layer's own pacing must give way: the frame-latency waitable object (`pacing::latency`,
`VICTORY_PRESENT_LATENCY`) and Reflex's sleep must not both wait; `slReflexSleep` should take the place of the
title's origin tick when Reflex is on. The cap stays meaningful for the real frames (FG doubles whatever comes
in). `VICTORY_PRESENT_SCALE` (`upscale.rs`) writes the back buffer at window size: the HUD-less colour must then
be at that size too (scale it with the same pass into a texture of ours, or keep present scaling off with FG).
NVIDIA Smooth Motion (the driver's frame generation, `play-nvidia.ps1` starts `victory_sm.exe` for it) must be
off with DLSS-G.

Order of work:
1. Verify step 1's map in the live frame with `VICTORY_DLSS=1` (Super Resolution or DLAA at the TAA pass).
2. Reflex/PCL markers alone (`kFeatureReflex`, `kFeaturePCL`), with the proxy queue and swap chain, no FG:
   proves the swap chain path and gives latency numbers.
3. DLSS-G with HUD-less colour, the interface plane, depth, motion, camera constants.

## Risks

- **Hardware GPU scheduling.** On the test PC Streamline answers `slIsFeatureSupported(kFeatureDLSS_G)` with
  `eErrorOSDisabledHWS` ("Feature 'kFeatureDLSS_G' requires GPU hardware scheduling to be enabled in the OS"),
  although the registry has `HwSchMode=2` (perhaps it waits for a restart). Frame Generation will not start until
  Windows reports it on. Super Resolution and Reflex do not need it (Reflex answered `ok`).
- **The upscale is not a TAAU here.** With `VICTORY_OWN_UPSCALER=1` and half scale the title runs its TAA at
  render size and scales up separately (step 1). DLSS Super Resolution in the right place needs the title's real
  TAAU pass (`TAA2U:CS_TAA2_Resolve`, present in the game files and never dispatched in the runs) or post
  processing at output size; see step 1.
- **Jitter, motion and depth conventions** are read from the live frame (step 1); a wrong sign or scale shows as
  smearing or ghosting, not as an error.
- **Exposure.** The colour is HDR before tone mapping; DLSS runs with auto exposure until the title's exposure
  texture (a 1x1 target the frame dump shows) is tagged.
- **The title's history.** DLSS overwrites only the pass's colour output; the title's other outputs (its history,
  statistics) stay the title's, so later passes are unaffected, but the title's TAA keeps running (its GPU cost
  stays).
- **Streamline's per-frame bookkeeping** runs in the present of a swap chain it knows; until the swap chain is
  Streamline's proxy it logs that `presentCommon()` is not observed, and its garbage collection does not run (memory
  may grow over long play with `VICTORY_DLSS=1`).
- **One device.** Streamline attaches to the first device it is given (`attach` refuses another); the layer makes
  one.
- **Driver and model files.** NGX caches models in `C:\ProgramData\NVIDIA\NGX\models`; a driver older than
  Streamline 2.14 needs fails `slInit` with "the driver is too old" (logged).
- **"Broken hair and missing letters"** (the earlier fork with DLSS): hair is drawn with transparency and
  dithering that TAA and DLSS treat as noise unless a reactive/transparency mask is given, and letters break when
  the interface is upscaled or interpolated with the scene. Here the interface is a separate plane composed after
  the upscale (`compose.rs`), so letters never pass through DLSS; hair needs `kBufferTypeTransparencyHint` or
  `kBufferTypeReactiveMaskHint` later.
