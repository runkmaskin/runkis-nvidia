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
3. **Real upscaling:** add `VICTORY_OWN_TAAU=1` (with the play script's `VICTORY_OWN_UPSCALER=1` and
   `VICTORY_RENDER_SCALE`): the title runs its own temporal upscaler to the output size and DLSS takes its place.
   Without it DLSS runs as DLAA at the render size. `VICTORY_DLSS_REFLEX=1` turns on Reflex and its markers,
   `VICTORY_DLSS_FG=1` checks frame generation (it needs hardware GPU scheduling).
4. `VICTORY_DLSS_PRESET=J|K|L|M` picks the model (K is DLSS 4's transformer, L and M DLSS 4.5's). Unset: NVIDIA's
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

**Motion vectors** are R16G16 float at render size, **in render pixels**: captured while the camera moved (run C1,
`VICTORY_DUMP_TARGETS`), the channels held -1.8 to 2.2 with means around 0.1, which as a fraction of the screen
would be two screens a frame; -10000 marks texels without a vector. The map gives DLSS `mvecScale` = 1/render
size. Their sign (current to previous or the other way) is not proven: the captures fell in the opening's slow
camera moves, where scripted stick input did not turn the camera, and the frames with DLSS showed no smearing at
half size. `VICTORY_DLSS_MOTION_SCALE=<x>,<y>` and `VICTORY_DLSS_JITTER_SCALE=<x>,<y>` change scale and sign without
a rebuild, for the check in play (see "Calibration" below).

**What picks TAA2U** (static reading of the 1.0.2.1 executable with Python and capstone, kept outside the
repository, confirmed in play). Each frame the title builds its TAA object from the scene's view (constructor at
`0x13f4540`, called from `0x2ecae1f`) and its setup (`0x13f3f60`, slot 2 of the own upscaler's vtable at
`0x6300bf0`) loads the `TAA2U` effect when the render size (`+0x44/+0x48`) differs from the scene's target size
(`+0x4c/+0x50`), else `TAA2`. `VICTORY_DATA_BREAK=x13f4023` read 960x540 against 960x540 with the layer's patches.
The target is the output size times the own upscaler's maximum scale, which the title takes as the larger of its
minimum and a per-console value (`0x1fb4300`: 1 on one profile, 2/3 on the other); `hooks.rs` made the maximum the
minimum to fix the scale. **`VICTORY_OWN_TAAU=1`** (new, off; with `VICTORY_OWN_UPSCALER=1` and
`VICTORY_RENDER_SCALE`) loads 1.0 there instead (`vmaxss xmm0, xmm0, [rsi+0x90]` at `0x28f1e87` becomes `vmovss
xmm0, [rip+disp]` reading this build's read-only 1.0 at `0x5fc6ed8`, eight bytes for eight). Measured in game (run
B, 540p): `TAA2U:CS_TAA2_Resolve` dispatched 240x135 groups of 8x8 every frame and `TAA2` never; post-processing
ran at 1920x1080; the picture was whole; 26.6-27.3 fps (one run, diagnostics build with the report on). The
upscaler's table layout is `TAA2`'s: colour 56, depth 32, motion 80 (960x540), history 112/136 and result 160 at
1920x1080, camera constants 168, sizes 176.
The pass is recognised at run time by its shader hash (the hash the log uses for every shader); the generic shape
rule (`taau::upscale_shape`) finds upscalers that write at output size and rightly did not take this TAA for one.

## Step 2 in the live frame

`VICTORY_DLSS=1` also puts DLSS into the frame (`draw.rs`, `State::dlss_after`, after the dispatch of the pass
`dlss::PASSES` names): the TAA pass runs as before, then DLSS is recorded from its colour (place 56), depth (32) and
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

Streamline logged once: "presentCommon() was not observed ... internal bookkeeping and garbage collection will not
run". Under manual hooking its per-frame bookkeeping runs in the present of a swap chain it knows. **Fixed:** with
`VICTORY_DLSS=1` the swap chain is made with Streamline's proxy factory (`dlss::proxy_factory`, `slUpgradeInterface`
in `Host::presenter`), so `Present` goes through Streamline. Run D: `DLSS: the swap chain is made through
Streamline's factory`, and the message is gone (the only Streamline error left is frame generation's, below).

**Real upscaling: `VICTORY_OWN_TAAU=1` with `VICTORY_DLSS=1`.** DLSS then follows `TAA2U` (`dlss::PASSES` maps both
passes; the mode follows the sizes) and writes the 1920x1080 result from the 960x540 inputs. Measured in game
(runs C1 and D, 540p): `DLSS: ... render=(960, 540) out=(1920, 1080) mode=Performance range=Ok([(960, 540), (960,
540), (1920, 1080)])`, every evaluation succeeded, the saved frames (the prison cell and yard, through the fence)
are whole and show no smearing at the half size the captures are saved at; 26.7-27.1 fps (one run, diagnostics
build; the GPU is not the limit, so DLSS costs no fps seen here). The title's own TAAU still runs before DLSS
(its history and statistics stay the title's); skipping it would save its GPU time only.

The other route (TAA off, DLSS at the spatial upscale draw `4f4ec7b16673362f` into a 1920x1080 texture of ours)
is not needed.

**Calibration** (step 2 of the follow-up; not finished). The scripted run reaches the opening's cutscenes, where
`L`/`J` did not turn the camera, so DLSS and the title's TAAU could not be compared on the same moving frames. To
finish: in free play, with `VICTORY_CAPTURE=<dir>` and F9 (or `VICTORY_CAPTURE_FRAMES`), pan with the mouse
(`VICTORY_MOUSE=1`) once with `VICTORY_DLSS=1` and once without, and look at edges against the sky and the fence:
trails behind moving edges mean the motion sign is wrong (`VICTORY_DLSS_MOTION_SCALE=-0.0010417,-0.0018519` at
960x540), a picture that shimmers when standing still means the jitter sign is wrong
(`VICTORY_DLSS_JITTER_SCALE=0.5,-0.5`).
## Steps 3 and 4: Frame Generation and Reflex (plan)

What Frame Generation (DLSS-G) needs, and where each piece lives in this layer:

| Needs | Where it is / would go |
|---|---|
| Streamline sees the swap chain made and every present | **Done** (`VICTORY_DLSS=1`, run D). `host.rs`, `Host::presenter`: `CreateDXGIFactory2` then `CreateSwapChainForHwnd(&self.queue, ...)`; `slUpgradeInterface(&factory)` first, so the swap chain is Streamline's proxy; `present_over` then calls `Present`, `GetBuffer`, `GetCurrentBackBufferIndex` and `ResizeBuffers` on the proxy. Everything else (the copy lists, `upscale.rs`, `compose.rs`) keeps the native objects (`slGetNativeInterface`). |
| The queue that presents made through Streamline (`eID3D12Device_CreateCommandQueue` is a mandatory hook) | `host.rs`, `create`: `device.CreateCommandQueue` for `Host::queue`. With FG: a proxy of the device (`slUpgradeInterface(&device)`) makes that one queue; the device itself stays native. |
| Features loaded: `kFeatureDLSS_G` (1000), `kFeatureReflex` (3), `kFeaturePCL` (4) | `dlss.rs`, `start`: the `features` list. `scripts\get-dlss.ps1` already fetches `sl.dlss_g.dll`, `nvngx_dlssg.dll`, `sl.reflex.dll`, `sl.pcl.dll`. |
| HUD-less colour (`kBufferTypeHUDLessColor`) at back-buffer size | `host.rs`, `present_over`: `texture` before `compose::record` is the game plane without the interface; the composed texture (`laid`) is what reaches the back buffer. Tag `texture` as HUD-less, valid until present. |
| The interface (`kBufferTypeUIColorAndAlpha`, optional) | `compose.rs`: the Series S interface plane `ui` (3840x2160, straight alpha) is separate already; tagging it lets DLSS-G keep the interface out of the interpolation, which is the classic cause of "broken letters" in generated frames. |
| Depth and motion vectors (render size, with their extent) | The TAA pass's own inputs (step 1 map): tag them at that dispatch with `eValidUntilPresent`, in the state they are left in, or copy them into textures of ours there (the title reuses its targets within the frame). |
| Camera matrices: `cameraViewToClip`, `clipToCameraView`, `clipToPrevClip`, `prevClipToClip`, camera position and axes, near, far, FOV | The TAA pass reprojects with the current and the previous view-projection: they are in its constant buffers (step 1 lists their words every 600 frames). Without jitter, row-major. `sl_matrix_helpers.h` shows how the previous-clip matrices are made from view and projection. |
| `slDLSSGSetOptions` (on, frames to generate, flags) | Once at start and when the window is resized (`presenter`). |
| Reflex / PCL markers per frame (`slPCLSetMarker`): simulation start/end, render submit start/end, present start/end; `slReflexSleep` | **Done** except `slReflexSleep` (`VICTORY_DLSS_REFLEX=1`: low latency mode, `dlss::marker`; run D: `Reflex: low latency mode on`, no marker refused). As built: simulation start where the title's frame-origin wait ends (`frame.rs`), simulation end and render submit start where its plane present reaches the layer (`queue.rs`, `show`), render submit end and present start/end around the host's `Present` (`host.rs`), all with `frame::current()` as the frame. Planned first: the title's frame-origin wait ends (`frame.rs`, the tick handed out: `wait`). Simulation end: the title's present call reaches the layer (`queue.rs`, the plane present). Render submit start/end: around the frame's submissions (`queue.rs` execute, the host `execute`). Present start/end: around `Present` in `present_over` (`host.rs`). `slReflexSleep` at the origin wait instead of (or before) the layer's own tick. |
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
1. Done: step 1's map in the live frame (DLAA at `TAA2`, Super Resolution at `TAA2U`).
2. Done: Reflex/PCL markers with the proxy swap chain (`VICTORY_DLSS_REFLEX=1`). Not done: `slReflexSleep`, latency
   numbers (`slReflexGetState`), the queue made through the device proxy.
3. `VICTORY_DLSS_FG=1` loads `sl.dlss_g` and logs whether frame generation can run; on the test PC it cannot
   (hardware GPU scheduling, below), so nothing more of it is wired: after the restart, next are the queue through
   `slUpgradeInterface(&device)`, `slDLSSGSetOptions`, frame-based tags (HUD-less game plane before `compose`,
   the interface plane, depth and motion of the TAA pass with `eValidUntilPresent`) and the camera constants from
   the TAA's buffer (place 168: view in words 0-15, projection 16-31; the previous frame's for `clipToPrevClip`),
   all under one frame token per title frame.

## Risks

- **Hardware GPU scheduling.** On the test PC Streamline answers `slIsFeatureSupported(kFeatureDLSS_G)` with
  `eErrorOSDisabledHWS` ("Feature 'kFeatureDLSS_G' requires GPU hardware scheduling to be enabled in the OS"),
  although the registry has `HwSchMode=2` (perhaps it waits for a restart). Frame Generation will not start until
  Windows reports it on. Super Resolution and Reflex do not need it (Reflex answered `ok`).
- **`VICTORY_OWN_TAAU` costs GPU time:** post-processing runs at 1080p instead of 540p. The GPU is not the limit on
  the test PC (no fps change seen in one run each); on a slower card it may be. It is a per-build patch (checked
  bytes, the 1.0.2.1 executable only).
- **Frame numbers.** DLSS evaluations use their own counter and the markers the title's frame count; both go to
  `slGetNewFrameToken`. For frame generation they must be one token per title frame.
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
