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

/// <summary>
/// One saved way to start the title. The base settings come from scripts\play-nvidia.ps1 (its switches are the
/// first group below); the overrides are settings the layer reads (each names the file that reads it) and are left
/// to the script when null.
/// </summary>
public sealed class LaunchProfile
{
    public const int CurrentSchema = 1;

    public int SchemaVersion { get; set; } = CurrentSchema;
    public string Name { get; set; } = "Default";

    // ---- The title and where its data lives ----
    /// <summary>The user's own game_scarlett_master.exe.</summary>
    public string GameExe { get; set; } = "";
    /// <summary>VICTORY_CACHE_DIR; null: the layer's default (%LOCALAPPDATA%\victory\cache).</summary>
    public string? CacheDir { get; set; }
    /// <summary>VICTORY_DATA_DIR (saves, crashes.log); null: the layer's default.</summary>
    public string? DataDir { get; set; }

    // ---- play-nvidia.ps1 switches ----
    public ConsoleModel Console { get; set; } = ConsoleModel.SeriesS;
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
    /// <summary>VICTORY_MOUSE=1 (gameinput.rs): the mouse turns the camera (right stick), left button fires, right aims.</summary>
    public bool Mouse { get; set; } = true;
    /// <summary>VICTORY_MOUSE_SENSE, with <see cref="Mouse"/> only (1: 12 pixels in 8 ms is a full stick).</summary>
    public double MouseSense { get; set; } = 1.0;
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

    public int OutputHeight => Console == ConsoleModel.SeriesX ? 2160 : 1080;
}
