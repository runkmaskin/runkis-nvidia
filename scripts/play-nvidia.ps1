# Starts the title on an NVIDIA card with the Series S settings measured on an RTX 5070 (about 18-22 fps, no device loss).
#   scripts\play-nvidia.ps1 -Exe <path of your own copy's executable>
# -NoSmoothMotion  run target\<build>\victory.exe itself. By default a copy named victory_sm.exe is started instead,
#                  so a Smooth Motion profile set for that exe in the NVIDIA App applies every time.
# -SeriesX         the Series X profile instead (slower: about 7 fps at half scale)
# -Quality         fewer stretched/jumping bodies and less rainbow skin, at about 13 fps instead of about 20
# -RayTracing      keep the title's ray-tracing passes (off by default: see below)
# -Resolution      the height the game renders at before the title's own upscaler: 2160, 1440, 1080, 720, 540 or 480.
#                  Series S outputs 1080p (so up to 1080 there), Series X 2160p. Default: half the output (540 on S,
#                  1080 on X). The play-S-*.ps1 and play-X-*.ps1 scripts next to this one call it with each.
# -Adapter         on a PC with more than one card, the one to use: part of its name (-Adapter 5060) or its index
#                  (-Adapter '#1'). Default: Windows' default card (the one the main display is on).
# -Build           target\diag (the default, the build all measurements are of) or target\release
#                  (the release build ignores the ray-tracing skip list: VICTORY_SKIP_PIPELINES is a diagnostics switch)
# -More            other settings: -More @{VICTORY_PANEL='1'}
# Run it from a normal window, not as administrator: the driver then keeps its shader cache elsewhere.
param(
    [Parameter(Mandatory)][string]$Exe,
    [switch]$NoSmoothMotion,
    [switch]$SeriesX,
    [switch]$Quality,
    [switch]$RayTracing,
    [ValidateSet('2160', '1440', '1080', '720', '540', '480')][string]$Resolution,
    [string]$Adapter,
    [ValidateSet('diag', 'release')][string]$Build = 'diag',
    [hashtable]$More = @{},
    [switch]$Precompile   # local: translate every shader with these exact settings (cache warm), no run
)
$settings = @{
    # This fork's play set (scripts\play.ps1, not -Safe): the speed set.
    VICTORY_TEXTURE_NO_WAIT = '1'; VICTORY_RELAY = '2'; VICTORY_CARD_STEPS = '2'
    VICTORY_WIDE_BACK = '1'; VICTORY_CARD_REHASH_RUNS = '1'
    VICTORY_JOIN_COPIES = '1'; VICTORY_INDIRECT_WINDOW = '1'; VICTORY_RELAY_COPY = '1'
    VICTORY_INDIRECT_BATCH = '2'; VICTORY_MAP_THREADS = '1'; VICTORY_TITLE_PRIORITIES = '0'; VICTORY_CARD_AWAKE = '1'
    VICTORY_SHOW_AFTER = '1'; VICTORY_AMBIENT = '0.8'; VICTORY_D3D12_DRED = $(if ($env:VN_DRED) { $env:VN_DRED } else { '0' }); VICTORY_SKIP_PIPELINES = 'none'
    # local: DRED 'light' cost an RTX 2070 SUPER 2-4x GPU time, so it is off ($env:VN_DRED='light' turns it on)
    VICTORY_THIN = '1'; VICTORY_JOIN_PARTS = '8'; VICTORY_UPLOAD_AHEAD = '1'
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
    VICTORY_PIPELINE_WAIT_MS = '10000'
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
}
if ($Adapter) { $settings.VICTORY_ADAPTER = $Adapter }
foreach ($k in $More.Keys) { $settings[$k] = $More[$k] }
$scale = [double]::Parse($settings.VICTORY_RENDER_SCALE, [Globalization.CultureInfo]::InvariantCulture)
Write-Host "Series $console, rendering at $([int]($outHeight * $scale))p (render scale $($settings.VICTORY_RENDER_SCALE))"

$victory = Join-Path $PSScriptRoot "..\target\$Build\victory.exe"
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
Push-Location (Join-Path $PSScriptRoot '..')   # victory.toml is looked for here
try { & $victory @runArgs } finally { Pop-Location }
