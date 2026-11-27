# Starts the title on an NVIDIA card with the Series S settings measured on an RTX 5070 (about 18-22 fps, no device loss).
#   scripts\play-nvidia.ps1 -Exe <path of your own copy's executable>
# -NoSmoothMotion  run target\<build>\victory.exe itself. By default a copy named victory_sm.exe is started instead,
#                  so a Smooth Motion profile set for that exe in the NVIDIA App applies every time.
# -SeriesX         the Series X profile instead (slower: about 7 fps at half scale)
# -Quality         fewer stretched/jumping bodies and less rainbow skin, at about 13 fps instead of about 20
# -RayTracing      keep the title's ray-tracing passes (off by default: see below)
# -Resolution      the height the game renders at before the title's own upscaler: 2160, 1440, 1080, 720, 540 or 480.
#                  Series S outputs 1080p (so up to 1080 there), Series X 2160p. Default: half the output (540 on S,
#                  1080 on X); on Series S 768 is the highest that starts (810+ runs the title out of its memory pool). The play-S-*.ps1 and play-X-*.ps1 scripts next to this one call it with each.
# -Adapter         on a PC with more than one card, the one to use: part of its name (-Adapter 5060) or its index
#                  (-Adapter '#1'). Default: Windows' default card (the one the main display is on).
# -Build           target\diag (the default, the build all measurements are of) or target\release
#                  (both builds honour the ray-tracing skip list since 56be6f9; DRED and debug-level logs need diag)
# -More            other settings: -More @{VICTORY_PANEL='1'}
# Run it from a normal window, not as administrator: the driver then keeps its shader cache elsewhere.
param(
    [Parameter(Mandatory)][string]$Exe,
    [switch]$NoSmoothMotion,
    [switch]$SeriesX,
    [switch]$Quality,
    [switch]$RayTracing,
    [ValidateSet('2160', '1440', '1080', '960', '900', '768', '720', '540', '480')][string]$Resolution,
    [string]$Adapter,
    [ValidateSet('diag', 'release', 'ab')][string]$Build = 'diag',
    [hashtable]$More = @{},
    [switch]$Precompile,  # local: translate every shader with these exact settings (cache warm), no run
    [switch]$Warm,        # local: build every recorded pipeline into the driver's cache (victory warm), no run
    [int]$WarmLimit = 0,  # local: with -Warm, at most this many pipelines (0: all)
    [switch]$PrintEnv     # launcher: print the settings, victory.exe path and arguments as JSON and stop. Nothing is
                          # copied, set or started, so the launcher runs the same settings as this script.
)
$settings = @{
    # This fork's play set (scripts\play.ps1, not -Safe): the speed set.
    VICTORY_TEXTURE_NO_WAIT = '1'; VICTORY_RELAY = '2'; VICTORY_CARD_STEPS = '2'
    VICTORY_WIDE_BACK = '1'; VICTORY_CARD_REHASH_RUNS = '1'
    VICTORY_JOIN_COPIES = '1'; VICTORY_INDIRECT_WINDOW = '1'; VICTORY_RELAY_COPY = '1'
    VICTORY_INDIRECT_BATCH = '2'; VICTORY_MAP_THREADS = '1'; VICTORY_TITLE_PRIORITIES = '0'; VICTORY_CARD_AWAKE = '0'
    # local: the keep-awake compute queue off: on an RTX 2070 SUPER 17.7 -> 18.7 fps, hitches over 100 ms 58 -> 33 (2026-11-27)
    VICTORY_SHOW_AFTER = '1'; VICTORY_AMBIENT = $(if ($env:VN_AMBIENT) { $env:VN_AMBIENT } else { '0.8' }); VICTORY_D3D12_DRED = $(if ($env:VN_DRED) { $env:VN_DRED } else { '0' }); VICTORY_SKIP_PIPELINES = 'none'
    # local: DRED 'light' cost an RTX 2070 SUPER 2-4x GPU time, so it is off ($env:VN_DRED='light' turns it on)
    VICTORY_THIN = '1'; VICTORY_JOIN_PARTS = '8'; VICTORY_UPLOAD_AHEAD = '1'
    # local: measured in game on 8d8db49 (5500X3D, 2 runs each against all-off 16.98 fps): fetches in four parts
    # 17.63 fps (waiting for fetches 10.1 -> 7.5 ms); the pool settled once a second 17.43 fps, ahead in three queues in a row
    VICTORY_FETCH_PARTS = '4'; VICTORY_POOL_SETTLE = '1'
    # local: measured in game 2026-11-27 (7800X3D, RTX 4070 SUPER, 540p, one run each, seconds 150-240): 25.95 fps,
    # 1 % low 8.0, 5 frames over 100 ms -> 27.14 fps, 1 % low 14.0, none; heap-record wait 4.2 -> 2.8 ms
    VICTORY_TABLE_CACHE = '1'; VICTORY_HEAP_ROUNDS_EARLY = '1'
    # local: the title's 30 fps lock (33,333 us frame interval) lifted to 60: 27.1 -> 35.5 fps at 540p in the same
    # stretch; the game keeps its speed at 60 (checked in play 2026-11-27)
    VICTORY_FRAME_INTERVAL_US = '16667'
    # local: host copies of placed textures whose memory is gone let go after 2.5 s: peak commit 24.5 -> 24.0-24.1 GB in
    # 5-minute runs, fps the same (27.4 vs 26.9, two runs each); the saving grows with play time
    VICTORY_TEXTURE_UNMAPPED_GO = '1'
    # local: the mouse turns the camera (right stick), left button fires, right aims (gameinput.rs)
    VICTORY_MOUSE = '1'
    # Cheap picture fixes, on in both sets:
    # - Overbright views: without rays the light probes keep the sky's light indoors. The fog is bounded by 1.5 times
    #   the light around, and what the title left by 2 times the estimate (was 4; ambient.rs).
    # (local: VN_AIR from STARTA-NVIDIA.bat overrides the 1.5; lower = less brown haze indoors)
    # local: 70 % of each pixel's light from the slow mean (whole picture pulsed with many people on screen)
    VICTORY_AMBIENT_STEADY = $(if ($env:VN_STEADY) { $env:VN_STEADY } else { '0.7' })
    VICTORY_AMBIENT_AIR = $(if ($env:VN_AIR) { $env:VN_AIR } else { '1.5' }); VICTORY_AMBIENT_MOST = '2'
    # - Black blobs: a NaN a shader writes is made 0 before blur and the temporal upscaler spread it (emit/lower.rs).
    VICTORY_NAN_SCRUB = '1'
    # Wait for new shaders rather than skip their draws (a hitch the first time a place is seen). Each pipeline is
    # waited for at most once (VICTORY_PIPELINE_WAIT_ONCE, on by default): one that takes longer no longer stops the
    # picture in every frame it is in view (pipeline/make.rs).
    # local: 0.5 s, not 10 s: a pipeline that takes longer (up to 5 s measured) shows a moment later instead of
    # freezing the picture (as Yaekai/victory-scarlett-3070 does with 20 ms). Test run 2026-11-27: looked right.
    VICTORY_PIPELINE_WAIT_MS = '500'
    # local: 8 GB card: the top level of textures 2048 px and larger stays off the card (Yaekai 707d7d9 port);
    # 1024 removed the card-full churn entirely but was visibly blurrier. Test run T1: 1 % low 7.2 fps (best).
    VICTORY_TEXTURE_MIP_DROP = '1'; VICTORY_TEXTURE_MIP_DROP_MIN_SIZE = '2048'
    # local: 6 threads for the big memory compares on a 12-thread CPU, not 3 (foxhound f502018)
    VICTORY_WIDE_THREADS = '6'
    # local: when free RAM falls under 4 GB, pool pages the title gave back are dropped from RAM (foxhound d2de2ea,
    # from Yaekai): idle on a PC with room, keeps a full 32 GB PC from paging (4-5 fps seen on one). Forced on always
    # in test Q7 it cost re-loads; at the default threshold it does nothing until RAM is short.
    VICTORY_POOL_DISCARD = '1'
    # local: 8 GB card: the texture queue keeps 1.5 GB free for render targets, not 3 GB (render targets alive at 540p:
    # ~1.1 GB). Q9: textures on the card 1.2 -> 2.3 GB, re-made 0.5-0.9 GB -> 0.07-0.2 GB, fps 14.7/12.5 -> 15.6/16.4,
    # worst frame 0.7-1.2 s -> 0.2 s; card in use 6.1 of 6.7-6.9 GB.
    VICTORY_CARD_ROOM_FOR_TARGETS_MB = '1536'
    # local: write down every pipeline the title creates (pipeline/known.rs), so -Warm can build them all into the
    # driver's cache before play: fewer hitches the first time a place is seen.
    VICTORY_PIPELINE_BOOK = '1'
    # NVIDIA caps the shader-visible view heap at 1,000,000: without more dynamic slots thousands of draws a frame
    # are dropped (descriptor/host.rs dynamic_slots).
    VICTORY_DYNAMIC_SLOTS = '524288'
    # Half render scale with the title's own upscaler (Series S needs it with a scale below 1; victory-loader hooks.rs).
    VICTORY_RENDER_SCALE = '0.5'; VICTORY_OWN_UPSCALER = '1'
    # Ray walks end at the root while the structure builder is off (emit/intersect.rs).
    VICTORY_BVH_MISS = '1'
    # Memory compares on the card instead of the CPU: 11 -> about 22 fps on Series S.
    VICTORY_CARD_BUFFERS_MIN_MB = '1'; VICTORY_CARD_BUFFERS_MB = '1536'
    # local: copies down to 64 KiB too (shadow.rs card_min): about +10 % on an RTX 2070 SUPER / Ryzen 5 5500X3D
    VICTORY_CARD_BUFFERS_MIN_KB = '64'
    # local: renderer ports 1, 2 and 6 from avatarmanz/VIctory-7900XTX (list calls 9.4 -> 6.8 us each here)
    VICTORY_PORT_CONSTANTS = '1'; VICTORY_PORT_DESCRIPTORS = '1'; VICTORY_PORT_GUARD = '1'
    # local: renderer port 5, constant buffers read in place by the translated shaders (prep 9.4 -> 7.7 ms per 1,000 draws here)
    # (deterministic test 2026-11-26: off 17.79 fps vs on 17.34; the GPU is the limit now and port 5 adds GPU work)
    VICTORY_PORT_CBV = '0'
    # local (after Neptune): guarded textures hashed every 32nd frame, not 8th: prep 47 -> 43 ms
    VICTORY_TEXTURE_HASH_EVERY = '32'
    # Series S presents the interface as a second plane: laid over the game (compose.rs), else black before gameplay.
    VICTORY_COMPOSE = '1'
}
# Ray tracing mostly off. The structure builder cannot run on NVIDIA (its 64-lane programs are refused; upstream
# Marvisak/VIctory pull request 2311), so every ray misses, and the passes that turn misses into light made rooms go
# white depending on the view angle. rt_off_hashes.txt lists the 1,000 CS_RaytracingResolve and 34 traced-shadow
# shaders, which are skipped. The screen-probe and visibility traces keep running: skipping those too left their
# outputs as garbage (rainbow glitter everywhere).
$rtList = Join-Path $PSScriptRoot 'rt_off_hashes.txt'
if (-not $RayTracing -and (Test-Path $rtList)) {
    $settings.VICTORY_SKIP_PIPELINES = (Get-Content $rtList -Raw).Trim()
    # local: the BVH skinning passes only feed the ray-tracing structures, which are never built here (VN_SKIP_BVH=1)
    $bvhList = Join-Path $PSScriptRoot 'skip_bvh.txt'
    if ($env:VN_SKIP_BVH -eq '1' -and (Test-Path $bvhList)) { $settings.VICTORY_SKIP_PIPELINES += ',' + (Get-Content $bvhList -Raw).Trim() }
    # local, optional: the strand hair's physics left out (VN_SKIP_HAIRSIM=1): hair does not sway, saves GPU time
    $hairList = Join-Path $PSScriptRoot 'skip_hairsim.txt'
    if ($env:VN_SKIP_HAIRSIM -eq '1' -and (Test-Path $hairList)) { $settings.VICTORY_SKIP_PIPELINES += ',' + (Get-Content $hairList -Raw).Trim() }
    # local (VN_LEAVE_OUT=1): two passes foxhound and Yaekai leave out by default (their VICTORY_LEAVE_OUT): the decal
    # heap's defragmentation (02384a60fda6f2d0, 11.85 ms of the card a frame) and the heat-haze / in-scattering pass
    # (731a39171b568298, smears blocks of pixels into a brown haze without ray-traced light).
    $leaveList = Join-Path $PSScriptRoot 'skip_leaveout.txt'
    if ($env:VN_LEAVE_OUT -eq '1' -and (Test-Path $leaveList)) { $settings.VICTORY_SKIP_PIPELINES += ',' + (Get-Content $leaveList -Raw).Trim() }
}
if ($Quality) {
    # Stretched/jumping bodies: relay stage 1 and no card steps (avatarmanz/VIctory-7900XTX issue 13,
    # fxceless/victory-fork issue 89). Rainbow skin: texture refreshes wait for the runtime's remapping.
    # Costs about 7 fps (the submitting thread waits about 30 ms a frame for the card).
    $settings.VICTORY_RELAY = '1'; $settings.VICTORY_CARD_STEPS = '0'; $settings.VICTORY_TEXTURE_NO_WAIT = '0'
}
# The render scale is the rendered height over the output height (Series S 1080, Series X 2160).
$outHeight = if ($SeriesX) { 2160 } else { 1080 }
$console = if ($SeriesX) { 'X' } else { 'S' }
if ($Resolution) {
    if ([int]$Resolution -gt $outHeight) {
        Write-Host "Series $console outputs ${outHeight}p: ${Resolution}p is above it."
        return
    }
    $settings.VICTORY_RENDER_SCALE = ([int]$Resolution / $outHeight).ToString('0.####', [Globalization.CultureInfo]::InvariantCulture)
    # local: above 768p the Series S title runs out of two start-up pools (graphics memory, physical pages); twice
    # their size lets 810-1080p start (victory-loader hooks.rs, VICTORY_TITLE_POOL; 1080p measured into gameplay)
    if (-not $SeriesX -and [int]$Resolution -gt 768) { $settings.VICTORY_TITLE_POOL = '2' }
}
if ($Adapter) { $settings.VICTORY_ADAPTER = $Adapter }
foreach ($k in $More.Keys) { $settings[$k] = $More[$k] }
$scale = [double]::Parse($settings.VICTORY_RENDER_SCALE, [Globalization.CultureInfo]::InvariantCulture)
if (-not $PrintEnv) { Write-Host "Series $console, rendering at $([int]($outHeight * $scale))p (render scale $($settings.VICTORY_RENDER_SCALE))" }

$victory = Join-Path $PSScriptRoot "..\target\$Build\victory.exe"
if ($PrintEnv) {
    $runArgs = @('run', $Exe, '--execute')
    if (-not $SeriesX) { $runArgs += @('--console', 'series-s') }
    if ($Precompile) { $runArgs = @('shaders', (Split-Path $Exe)) }
    if ($Warm) { $runArgs = @('warm'); if ($WarmLimit -gt 0) { $runArgs += @('--limit', "$WarmLimit") } }
    $sorted = [ordered]@{}
    foreach ($k in ($settings.Keys | Sort-Object)) { $sorted[$k] = [string]$settings[$k] }
    [ordered]@{
        victory      = [IO.Path]::GetFullPath($victory)
        workdir      = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
        smoothMotion = -not $NoSmoothMotion
        args         = $runArgs
        settings     = $sorted
    } | ConvertTo-Json -Depth 4 -Compress
    return
}
if (-not (Test-Path $victory)) { Write-Host "No $victory. Build it first (README.md)."; return }
if (-not (Test-Path $Exe)) { Write-Host "No $Exe."; return }
if (-not $NoSmoothMotion) {
    # A copy under the name the NVIDIA App's Smooth Motion profile is set for, refreshed when the build changes.
    $sm = Join-Path (Split-Path $victory) 'victory_sm.exe'
    if (-not (Test-Path $sm) -or (Get-Item $sm).LastWriteTime -lt (Get-Item $victory).LastWriteTime) {
        Copy-Item $victory $sm -Force
    }
    $victory = $sm
}
# (Other settings left over in this window go; where your files are stays.)
$paths = 'VICTORY_CONFIG', 'VICTORY_GAME_DIR', 'VICTORY_CACHE_DIR', 'VICTORY_DATA_DIR'
Get-ChildItem Env:VICTORY_* | Where-Object { $_.Name -notin $paths } | ForEach-Object { Remove-Item "Env:$($_.Name)" }
foreach ($k in $settings.Keys) { Set-Item "Env:$k" $settings[$k] }
$Exe = (Resolve-Path $Exe).Path
$runArgs = @('run', $Exe, '--execute')
if (-not $SeriesX) { $runArgs += @('--console', 'series-s') }
if ($Precompile) { $runArgs = @('shaders', (Split-Path $Exe)) }
if ($Warm) { $runArgs = @('warm'); if ($WarmLimit -gt 0) { $runArgs += @('--limit', "$WarmLimit") } }
Push-Location (Join-Path $PSScriptRoot '..')   # victory.toml is looked for here
try { & $victory @runArgs } finally { Pop-Location }
