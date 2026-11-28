using System.Text.RegularExpressions;

namespace Runkis.Launcher.Core;

public sealed record ValidationIssue(string Field, string Message, bool Blocking = true);

/// <summary>The ranges the layer itself accepts (or clamps to); a value outside them is reported, not silently changed.</summary>
public static partial class ProfileValidator
{

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

        if (p.FrameGen != 0 && p.FrameGen is < 2 or > 6) Add(nameof(p.FrameGen), "Frame generation is off (0) or 2 to 6 frames shown for each one rendered.");
        if (p.Fps is < 1 or > 1000) Add(nameof(p.Fps), "The frame rate cap is 1 to 1000 (1000: none; 30: the game's own).");
        if (p.TitlePool is < 0 or > 16) Add(nameof(p.TitlePool), "The title pool factor is 0 (off) to 16.");
        if (p.TitlePriorityCap is < -2 or > 2) Add(nameof(p.TitlePriorityCap), "The title thread cap is -2 (lowest) to 2 (highest), or unset.");
        if (p.FrameGen != 0 && p.Dlss == DlssMode.Off)
            Add(nameof(p.FrameGen), "Frame generation needs DLSS on: with DLSS off victory play does not make it ready.", blocking: false);
        if (p.Console == ConsoleModel.SeriesS && p.Dlss == DlssMode.Dlaa && p.TitlePool == 0)
            Add(nameof(p.TitlePool), "Series S with DLAA draws 1080 lines; above 768 the game runs out of its Series S pools. Set the title pool to 2.", blocking: false);

        if (p.Adapter.Length > 0 && p.Adapter.StartsWith('#') && !AdapterIndex().IsMatch(p.Adapter))
            Add(nameof(p.Adapter), "An adapter index is written #0, #1, ...");

        // NVIDIA's files: `victory get-dlss` fetches them (victory play asks once at a start without them).
        if (p.Dlss is not DlssMode.Off and not null && !File.Exists(Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "victory", "nvidia-sdk", "Streamline", "bin", "x64", "sl.interposer.dll")))
            issues.Add(new(nameof(p.Dlss), "DLSS needs NVIDIA's files: run `victory get-dlss` once (into %LOCALAPPDATA%\\victory\\nvidia-sdk). Without them the title's upscaler is used.", false));
        Range(issues, nameof(p.PipelineWaitMs), p.PipelineWaitMs, 0, 20000);
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

        _ = monitors; // (the window opens on the main monitor on this base)
        Range(issues, nameof(p.MouseSense), p.MouseSense, 0.1, 10);
        if (p.Mouse && p.MouseCapture)
            Add(nameof(p.MouseCapture), "With the mouse as the right stick the game's own mouse is not fed; the cursor is still held in the window.", blocking: false);
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
