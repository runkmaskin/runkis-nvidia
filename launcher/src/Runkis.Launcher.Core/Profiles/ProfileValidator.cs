using System.Text.RegularExpressions;

namespace Runkis.Launcher.Core;

public sealed record ValidationIssue(string Field, string Message, bool Blocking = true);

/// <summary>The ranges the layer itself accepts (or clamps to); a value outside them is reported, not silently changed.</summary>
public static partial class ProfileValidator
{
    /// <summary>The heights play-nvidia.ps1 accepts for -Resolution.</summary>
    // Above 768 on Series S the script turns on VICTORY_TITLE_POOL=2 (the title's start-up pools, twice their size).
    public static readonly int[] Resolutions = [2160, 1440, 1080, 960, 900, 768, 720, 540, 480];

    [GeneratedRegex(@"^[A-Za-z0-9_:=,.\-]+$")]
    private static partial Regex FilterChars();

    [GeneratedRegex(@"^#\d+$")]
    private static partial Regex AdapterIndex();

    /// <param name="monitors">The monitors connected now, to warn about a monitor number past them; null: not checked.</param>
    public static List<ValidationIssue> Validate(LaunchProfile p, RepoLayout? repo = null, IReadOnlyList<DisplayMonitor>? monitors = null)
    {
        var issues = new List<ValidationIssue>();
        void Add(string f, string m, bool blocking = true) => issues.Add(new(f, m, blocking));

        if (string.IsNullOrWhiteSpace(p.Name)) Add(nameof(p.Name), "The profile needs a name.");
        else if (p.Name.Trim().IndexOfAny(Path.GetInvalidFileNameChars()) >= 0)
            Add(nameof(p.Name), "The name has characters a file name cannot have.");

        if (string.IsNullOrWhiteSpace(p.GameExe)) Add(nameof(p.GameExe), "Choose your copy of game_scarlett_master.exe.");
        else if (!File.Exists(p.GameExe)) Add(nameof(p.GameExe), $"{p.GameExe} does not exist.");
        else if (!p.GameExe.EndsWith(".exe", StringComparison.OrdinalIgnoreCase)) Add(nameof(p.GameExe), "That is not an .exe file.");

        if (p.Resolution is int r)
        {
            if (Array.IndexOf(Resolutions, r) < 0) Add(nameof(p.Resolution), $"{r}p is not one of {string.Join(", ", Resolutions)}.");
            else if (r > p.OutputHeight)
                Add(nameof(p.Resolution), $"Series {(p.Console == ConsoleModel.SeriesX ? "X" : "S")} outputs {p.OutputHeight}p: {r}p is above it.");
        }

        if (p.Adapter.Length > 0 && p.Adapter.StartsWith('#') && !AdapterIndex().IsMatch(p.Adapter))
            Add(nameof(p.Adapter), "An adapter index is written #0, #1, ...");

        Range(issues, nameof(p.Air), p.Air, 0, 3);
        Range(issues, nameof(p.Steady), p.Steady, 0, 1);
        Range(issues, nameof(p.Ambient), p.Ambient, 0, 2);
        Range(issues, nameof(p.PresentSharpen), p.PresentSharpen, 0, 1);
        Range(issues, nameof(p.MouseSense), p.MouseSense, 0.1, 10);
        // NVIDIA's files are fetched by scripts\get-dlss.ps1, never kept in the repository.
        if (p.Upscaler == UpscalerMode.Dlss && !File.Exists(Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "victory", "dlss", "nvngx_dlss.dll")))
            issues.Add(new(nameof(p.Upscaler), "DLSS needs NVIDIA's files: run scripts\\get-dlss.ps1 once (it downloads them into %LOCALAPPDATA%\\victory\\dlss). Without them the title's upscaler is used.", false));
        Range(issues, nameof(p.PipelineWaitMs), p.PipelineWaitMs, 0, 20000);
        Range(issues, nameof(p.MipDrop), p.MipDrop, 0, 3);
        Range(issues, nameof(p.MipDropMinSize), p.MipDropMinSize, 1, 16384);
        Range(issues, nameof(p.WideThreads), p.WideThreads, 1, 16);
        Range(issues, nameof(p.TargetRoomMb), p.TargetRoomMb, 0, 8192);
        Range(issues, nameof(p.WarmLimit), p.WarmLimit, 0, 1_000_000);

        if (string.IsNullOrWhiteSpace(p.LogFilter) || !FilterChars().IsMatch(p.LogFilter))
            Add(nameof(p.LogFilter), "A log filter looks like info, warn or victory_gpu=debug,info.");
        else if (p.Build == BuildKind.Release && (p.LogFilter.Contains("debug") || p.LogFilter.Contains("trace")))
            Add(nameof(p.LogFilter), "A release build logs info and above only; debug and trace need the diag build.", blocking: false);

        if (p.Build == BuildKind.Release && p.Dred != DredMode.Off)
            Add(nameof(p.Dred), "Device-removed data (DRED) exists only in the diag build; the release build ignores it.", blocking: false);
        if (p.Dred == DredMode.Full)
            Add(nameof(p.Dred), "Full DRED cost an RTX 2070 SUPER 2-4x GPU time; use it only to find a device loss.", blocking: false);

        if (!string.IsNullOrWhiteSpace(p.WindowSize))
        {
            if (!Displays.TryParseSize(p.WindowSize, out _, out _))
                Add(nameof(p.WindowSize), "A window size is written WIDTHxHEIGHT, like 1600x900.");
            else if (p.Window != WindowMode.Windowed)
                Add(nameof(p.WindowSize), "The window size applies to Windowed only; borderless and full screen cover the monitor.", blocking: false);
        }
        if (!Enum.IsDefined(p.Window)) Add(nameof(p.Window), "Display mode is Windowed, Borderless or Fullscreen.");
        if (p.Monitor is < 0 or > 16) Add(nameof(p.Monitor), "Must be between 0 (the main monitor) and 16.");
        else if (p.Monitor > 0 && monitors is { Count: > 0 } && p.Monitor > monitors.Count)
            Add(nameof(p.Monitor), $"Monitor {p.Monitor} is not connected ({monitors.Count} found): the game opens on the main monitor.", blocking: false);

        issues.AddRange(KeyMap.Check(p));

        foreach (var (field, dir) in new[] { (nameof(p.CacheDir), p.CacheDir), (nameof(p.DataDir), p.DataDir) })
        {
            if (string.IsNullOrWhiteSpace(dir)) continue;
            if (!Path.IsPathFullyQualified(dir)) Add(field, "Give a full path, like D:\\victory-data\\cache.");
            else if (repo != null && repo.Contains(dir))
                Add(field, "The layer refuses folders inside the source tree for game-derived data (docs\\asset-policy.md).");
        }
        return issues;
    }

    private static void Range(List<ValidationIssue> issues, string field, double? value, double min, double max)
    {
        if (value is double v && (double.IsNaN(v) || v < min || v > max))
            issues.Add(new(field, $"Must be between {min} and {max}."));
    }
}
