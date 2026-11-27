using System.Text.Json;
using System.Text.Json.Serialization;

namespace Runkis.Launcher.Core;

public enum ConsoleModel { SeriesS, SeriesX }
public enum BuildKind { Diag, Release }
public enum DredMode { Off, Light, Full }
public enum RunMode { Play, Warm, Precompile }

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
