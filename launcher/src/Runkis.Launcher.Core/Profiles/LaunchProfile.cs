using System.Text.Json;
using System.Text.Json.Serialization;

namespace Runkis.Launcher.Core;

public enum ConsoleModel { SeriesS, SeriesX }
public enum BuildKind { Diag, Release }
public enum DredMode { Off, Light, Full }
public enum RunMode { Play, Warm, Precompile }
/// <summary>VICTORY_WINDOW (display.rs): a framed window, a borderless one covering the monitor, or full screen (borderless too).</summary>
public enum WindowMode { Windowed, Borderless, Fullscreen }

/// <summary>What scales the rendered picture to the game's 1080p plane.</summary>
public enum UpscalerMode
{
    /// <summary>The title's anti-aliasing at render size, then a spatial upscale (as before).</summary>
    Game,
    /// <summary>The title's own temporal upscaler (VICTORY_OWN_TAAU=1).</summary>
    GameTemporal,
    /// <summary>NVIDIA DLSS in place of the title's temporal upscaler, with Reflex (VICTORY_DLSS=1).</summary>
    Dlss,
}

/// <summary>`victory play --dlss`: DLSS Super Resolution's mode, or off (the title's own upscaling).</summary>
public enum DlssMode { Off, Dlaa, Quality, Balanced, Performance }
/// <summary>`victory play --res` (Series X only): 2560x1440 output and scene, 1080p scene to 1440p, or the stock 4K output.</summary>
public enum SeriesXPicture { P1440, P1080To1440, P4k }
/// <summary>`victory play --fg-type`: which frame generation.</summary>
public enum FrameGenType { Auto, Dlss, Fsr }

/// <summary>
/// One saved way to start the title. The base settings are `victory play`'s own (apps/victory/src/play.rs; the
/// launcher reads them with <c>victory play --print-env</c>); the play options are the second group below, and the
/// overrides are switches the layer reads, given as <c>--set</c> and left to the layer when null.
/// Schema 1 profiles (the old base, before 2026-11-28) are read too: <see cref="Normalize"/> turns their picture
/// choices into play options; their stand-in light, skips, upscale and key bindings are not used on this base.
/// </summary>
public sealed class LaunchProfile
{
    public const int CurrentSchema = 2;

    public int SchemaVersion { get; set; } = CurrentSchema;
    public string Name { get; set; } = "Default";

    // ---- The title and where its data lives ----
    /// <summary>The user's own game_scarlett_master.exe.</summary>
    public string GameExe { get; set; } = "";
    /// <summary>VICTORY_CACHE_DIR; null: the layer's default (%LOCALAPPDATA%\victory\cache).</summary>
    public string? CacheDir { get; set; }
    /// <summary>VICTORY_DATA_DIR (saves, crashes.log); null: the layer's default.</summary>
    public string? DataDir { get; set; }

    // ---- victory play options ----
    /// <summary>`--series-s` when Series S (the launcher's default: 1080p output, faster); Series X (2560x1440 with DLSS) is `victory play`'s own default.</summary>
    public ConsoleModel Console { get; set; } = ConsoleModel.SeriesS;
    /// <summary>`--dlss`; null (a schema 1 profile): worked out by <see cref="Normalize"/>.</summary>
    public DlssMode? Dlss { get; set; } = DlssMode.Quality;
    /// <summary>`--res`, Series X only.</summary>
    public SeriesXPicture Picture { get; set; } = SeriesXPicture.P1440;
    /// <summary>`--framegen`: frames shown for each one rendered, 2 to 6; 0: ready but off (the F1 menu switches it).</summary>
    public int FrameGen { get; set; }
    /// <summary>`--fg-type`.</summary>
    public FrameGenType FgType { get; set; } = FrameGenType.Auto;
    /// <summary>`--fps`: frames a second at most, 1 to 1000 (1000: no cap; 30: the title's own rate).</summary>
    public int Fps { get; set; } = 1000;
    /// <summary>VICTORY_FENCE_SLEEP=1 (victory-loader fence_wait.rs): the title's frame fence wait blocks instead of spinning.</summary>
    public bool FenceSleep { get; set; }
    /// <summary>
    /// VICTORY_TITLE_POOL (victory-loader title_pool.rs): the title's Series S pools this many times larger (0: as they are).
    /// Series S above 768 lines (DLAA, 1080 lines) needs 2.
    /// </summary>
    public int TitlePool { get; set; }
    /// <summary>
    /// VICTORY_TITLE_PRIORITY_CAP (victory-hle kernel.rs): the title's raised threads (its job pools run 13 workers at
    /// "highest") at this priority at most, so the layer and the desktop keep processors; null: as the title sets them.
    /// </summary>
    public int? TitlePriorityCap { get; set; }
    /// <summary>VICTORY_FULLSCREEN=1 (victory-hle lifecycle.rs): a borderless window covering the monitor.</summary>
    public bool? Fullscreen { get; set; }

    // ---- Schema 1 (the old base): read, kept in the file, not used on this base except as Normalize says ----
    /// <summary>-Resolution: rendered height before the title's upscaler; null: half the output.</summary>
    public int? Resolution { get; set; } = 540;
    public bool Quality { get; set; }
    public bool RayTracing { get; set; }
    /// <summary>-Adapter / VICTORY_ADAPTER: part of a card's name, or "#n" (DXGI order); empty: Windows' default card.</summary>
    public string Adapter { get; set; } = "";
    public BuildKind Build { get; set; } = BuildKind.Diag;
    /// <summary>Start a copy named victory_sm.exe, so an NVIDIA App Smooth Motion profile for that name applies.</summary>
    public bool SmoothMotionCopy { get; set; }
    /// <summary>-WarmLimit for the pipeline warm-up (0: all).</summary>
    public int WarmLimit { get; set; }
    /// <summary>
    /// The full warm-up: every pipeline earlier plays recorded (-Warm alone), not only those they drew with
    /// (-Warm -WarmUsed, the fast one). Chosen per warm-up, not saved.
    /// </summary>
    [JsonIgnore]
    public bool WarmAll { get; set; }

    // ---- Picture (VN_* variables the script turns into VICTORY_* settings) ----
    /// <summary>VN_AIR -> VICTORY_AMBIENT_AIR: how much the air may scatter (indoor haze).</summary>
    public double Air { get; set; } = 0.3;
    /// <summary>VN_STEADY -> VICTORY_AMBIENT_STEADY (0..1).</summary>
    public double Steady { get; set; } = 0.7;
    /// <summary>VN_AMBIENT -> VICTORY_AMBIENT: the stand-in light level.</summary>
    public double Ambient { get; set; } = 0.8;
    public bool SkipBvh { get; set; } = true;
    public bool SkipHairSim { get; set; }
    public bool LeaveOut { get; set; }
    /// <summary>VICTORY_PRESENT_SCALE=1 (upscale.rs): the 1080p frame scaled to the window with a sharp filter, not stretched by Windows.</summary>
    public bool PresentScale { get; set; }
    /// <summary>VICTORY_PRESENT_SHARPEN (0..1), with <see cref="PresentScale"/> only.</summary>
    public double PresentSharpen { get; set; } = 0.3;
    /// <summary>VICTORY_OWN_TAAU / VICTORY_DLSS (docs/dlss.md): the upscaler.</summary>
    public UpscalerMode Upscaler { get; set; } = UpscalerMode.Game;
    // ---- Display (display.rs; the defaults set nothing, so the window is made as before) ----
    /// <summary>VICTORY_WINDOW: set only when not <see cref="WindowMode.Windowed"/>.</summary>
    public WindowMode Window { get; set; } = WindowMode.Windowed;
    /// <summary>VICTORY_WINDOW_SIZE ("1600x900", the client area) for a framed window; empty: the game's own size, fitted to the screen.</summary>
    public string WindowSize { get; set; } = "";
    /// <summary>VICTORY_MONITOR: 1 for the first monitor Windows lists; 0: the main monitor.</summary>
    public int Monitor { get; set; }
    /// <summary>VICTORY_MOUSE=1 (gameinput.rs): the mouse turns the camera as the stand-in pad's right stick, left button fires, right aims.</summary>
    public bool Mouse { get; set; } = true;
    /// <summary>VICTORY_MOUSE_CAPTURE=1 (upstream's lifecycle.rs): the game's own mouse, the cursor held in its window.</summary>
    public bool MouseCapture { get; set; }
    /// <summary>VICTORY_MOUSE_SENSE (1: for the stick, 12 pixels in one 8 ms poll is a full stick; for the native mouse, the delta as it is).</summary>
    public double MouseSense { get; set; } = 1.0;
    /// <summary>VICTORY_MOUSE_INVERT_Y=1: up on the mouse looks down.</summary>
    public bool MouseInvertY { get; set; }
    /// <summary>
    /// VICTORY_KEYMAP (gameinput.rs): pad target ("A", "LT", "LSUp", ...) to its keys ("C|Mouse1", "" for none),
    /// see <see cref="KeyMap"/>. null, or a target left out: the layer's default.
    /// </summary>
    public Dictionary<string, string>? KeyMap { get; set; }

    // ---- Performance overrides (null: what the script sets) ----
    /// <summary>VICTORY_PIPELINE_WAIT_MS (pipeline/make.rs).</summary>
    public int? PipelineWaitMs { get; set; }
    /// <summary>VICTORY_TEXTURE_MIP_DROP, 0..3 (resource.rs).</summary>
    public int? MipDrop { get; set; }
    /// <summary>VICTORY_TEXTURE_MIP_DROP_MIN_SIZE in pixels (resource.rs).</summary>
    public int? MipDropMinSize { get; set; }
    /// <summary>VICTORY_WIDE_THREADS, 1..16 (shadow.rs).</summary>
    public int? WideThreads { get; set; }
    /// <summary>VICTORY_CARD_ROOM_FOR_TARGETS_MB, 0..8192 (resource.rs).</summary>
    public int? TargetRoomMb { get; set; }
    /// <summary>VICTORY_POOL_DISCARD (xmem.rs).</summary>
    public bool? PoolDiscard { get; set; }
    /// <summary>VICTORY_PIPELINE_BOOK (pipeline/known.rs): record pipelines so a warm-up can build them.</summary>
    public bool? PipelineBook { get; set; }
    /// <summary>VICTORY_D3D12_BACKGROUND=0 switches the driver's background shader work off (host.rs).</summary>
    public bool D3D12BackgroundOff { get; set; }

    // ---- Diagnostics ----
    /// <summary>VN_DRED -> VICTORY_D3D12_DRED (diag builds only).</summary>
    public DredMode Dred { get; set; } = DredMode.Off;
    /// <summary>VICTORY_PANEL: the on-screen numbers panel shown from the start (F3 toggles it anyway).</summary>
    public bool Panel { get; set; }
    /// <summary>VICTORY_LOG: the tracing filter (debug and trace need a diag build).</summary>
    public string LogFilter { get; set; } = "info";

    internal static readonly JsonSerializerOptions Json = new()
    {
        WriteIndented = true,
        Converters = { new JsonStringEnumConverter() },
        DefaultIgnoreCondition = JsonIgnoreCondition.Never,
    };

    public LaunchProfile Clone() => JsonSerializer.Deserialize<LaunchProfile>(JsonSerializer.Serialize(this, Json), Json)!;

    /// <summary>The title's output height before any upscale of ours: Series S 1080, Series X 1440 (or 2160 at 4K).</summary>
    public int OutputHeight => Console == ConsoleModel.SeriesS ? 1080 : Picture == SeriesXPicture.P4k ? 2160 : 1440;

    /// <summary>
    /// A schema 1 profile's choices as play options: DLSS from its upscaler, in the mode whose scene is nearest its
    /// render height (Series S "768p" is DLSS Quality, 720 lines of 1080), and full screen from its display mode.
    /// The schema is raised, so this runs once; a profile that has the play options keeps them.
    /// </summary>
    public LaunchProfile Normalize()
    {
        if (SchemaVersion < 2)
        {
            // (A schema 1 file has no play options: they hold the new defaults until set here.)
            Dlss = Upscaler == UpscalerMode.Dlss ? NearestDlss(Resolution, Console == ConsoleModel.SeriesS ? 1080 : 1440) : DlssMode.Off;
            Fullscreen = Window != WindowMode.Windowed;
            SchemaVersion = CurrentSchema;
        }
        Dlss ??= DlssMode.Quality;
        return this;
    }

    /// <summary>The DLSS mode whose scene (scale of <paramref name="output"/>) is nearest <paramref name="height"/>.</summary>
    public static DlssMode NearestDlss(int? height, int output)
    {
        if (height is not int h || output <= 0) return DlssMode.Quality;
        var scale = (double)h / output;
        (DlssMode Mode, double Scale)[] modes = [(DlssMode.Dlaa, 1.0), (DlssMode.Quality, 2.0 / 3), (DlssMode.Balanced, 0.58), (DlssMode.Performance, 0.5)];
        return modes.MinBy(m => Math.Abs(m.Scale - scale)).Mode;
    }
}
