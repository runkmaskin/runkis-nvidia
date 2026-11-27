namespace Runkis.Launcher.Core;

/// <summary>A ready-made way to start, applied over the script's tuned set. Each one cites what was measured.</summary>
public sealed record Preset(string Id, string Name, string Description, Action<LaunchProfile> Apply);

public static class Presets
{
    public static readonly IReadOnlyList<Preset> All =
    [
        new("standard", "Standard",
            "The measured PLAY.bat set: Series S at 540p with the title's own upscaler. About 18 fps on a Ryzen 5 5500X3D + RTX 2070 SUPER.",
            _ => { }),
        new("fps", "Max FPS",
            "480p, hair physics left out, and the decal defragmentation and heat-haze passes left out (about 12 ms of GPU time a frame). "
            + "Helps most when the graphics card is the limit; little when the CPU is.",
            p => { p.Resolution = 480; p.SkipHairSim = true; p.LeaveOut = true; }),
        new("sharp", "Sharp (10 GB+ VRAM)",
            "720p and full-size textures: the 8 GB-card limits (texture mip drop, small render-target reserve) are lifted. "
            + "For cards with 10 GB or more; costs some fps for the higher resolution.",
            p => { p.Resolution = 720; p.MipDrop = 0; p.TargetRoomMb = 3072; }),
        new("quality", "Fewer glitches",
            "Quality mode: stops stretched or jumping bodies, textures through walls and rainbow skin. About 29 % fewer fps.",
            p => { p.Quality = true; }),
        new("seriesx", "Series X",
            "The larger console profile at half its 2160p output (1080p). More detail, much slower: about 7 fps on an RTX 2070 SUPER.",
            p => { p.Console = ConsoleModel.SeriesX; p.Resolution = null; }),
        new("safe", "Troubleshoot",
            "For crashes and GPU resets: Quality mode, every new shader waited for instead of skipped, and GPU crash data in the log "
            + "(diag build). Slow; use it to find a problem, not to play.",
            p => { p.Quality = true; p.PipelineWaitMs = 10000; p.Dred = DredMode.Light; p.Build = BuildKind.Diag; }),
    ];

    public static Preset? ById(string? id) => All.FirstOrDefault(p => p.Id == id);

    /// <summary>
    /// The preset as a profile: the user's game, folders, card, build and log filter kept from <paramref name="basis"/>,
    /// everything else from the script's defaults and the preset.
    /// </summary>
    public static LaunchProfile Make(Preset preset, LaunchProfile basis)
    {
        var p = new LaunchProfile
        {
            Name = preset.Name,
            GameExe = basis.GameExe,
            CacheDir = basis.CacheDir,
            DataDir = basis.DataDir,
            Adapter = basis.Adapter,
            Build = basis.Build,
            LogFilter = basis.LogFilter,
            WarmLimit = basis.WarmLimit,
        };
        preset.Apply(p);
        return p;
    }
}
