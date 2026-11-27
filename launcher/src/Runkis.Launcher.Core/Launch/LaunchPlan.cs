using System.Diagnostics;
using System.Globalization;
using System.Text;
using System.Text.Json;

namespace Runkis.Launcher.Core;

/// <summary>Everything needed to start one run, fixed before anything starts.</summary>
public sealed class LaunchPlan
{
    public required RunMode Mode { get; init; }
    public required string Victory { get; init; }
    public required string WorkDir { get; init; }
    public required IReadOnlyList<string> Args { get; init; }
    /// <summary>The VICTORY_* settings (and NO_COLOR) the run gets; everything else is inherited.</summary>
    public required IReadOnlyDictionary<string, string> Settings { get; init; }
    /// <summary>The settings that came from the profile on top of the script's, for display.</summary>
    public required IReadOnlyCollection<string> Overridden { get; init; }
    public required string LogPath { get; init; }
    public string? FrameLogPath { get; init; }
    public required string GameExe { get; init; }
    public required string DataDir { get; init; }
    public required string CacheDir { get; init; }

    public string CommandLine => Quote(Victory) + " " + string.Join(" ", Args.Select(Quote));

    private static string Quote(string a) => a.Contains(' ') ? $"\"{a}\"" : a;
}

/// <summary>What play-nvidia.ps1 -PrintEnv answers.</summary>
public sealed class ScriptPreset
{
    public string Victory { get; set; } = "";
    public string Workdir { get; set; } = "";
    public bool SmoothMotion { get; set; }
    public List<string> Args { get; set; } = [];
    public Dictionary<string, string> Settings { get; set; } = [];
}

public sealed class PlanException(string message) : Exception(message);

/// <summary>
/// Builds a <see cref="LaunchPlan"/>: the script decides the tuned base settings (one source of truth with PLAY.bat),
/// the profile's overrides go on top, and the paths and logs are the launcher's.
/// </summary>
public sealed class PlanBuilder(RepoLayout repo)
{
    /// <summary>The switches play-nvidia.ps1 is called with for this profile and mode.</summary>
    public static List<string> ScriptArgs(LaunchProfile p, RunMode mode)
    {
        var a = new List<string> { "-Exe", p.GameExe, "-Build", RepoLayout.BuildFolder(p.Build) };
        if (p.Console == ConsoleModel.SeriesX) a.Add("-SeriesX");
        if (p.Quality) a.Add("-Quality");
        if (p.RayTracing) a.Add("-RayTracing");
        if (p.Resolution is int r) a.AddRange(["-Resolution", r.ToString(CultureInfo.InvariantCulture)]);
        if (!string.IsNullOrWhiteSpace(p.Adapter)) a.AddRange(["-Adapter", p.Adapter.Trim()]);
        if (!p.SmoothMotionCopy) a.Add("-NoSmoothMotion");
        if (mode == RunMode.Warm)
        {
            a.Add("-Warm");
            if (p.WarmLimit > 0) a.AddRange(["-WarmLimit", p.WarmLimit.ToString(CultureInfo.InvariantCulture)]);
        }
        if (mode == RunMode.Precompile) a.Add("-Precompile");
        a.Add("-PrintEnv");
        return a;
    }

    /// <summary>The VN_* variables the script reads (PLAY.bat sets the same ones).</summary>
    public static Dictionary<string, string> ScriptVariables(LaunchProfile p) => new()
    {
        ["VN_AIR"] = Num(p.Air),
        ["VN_STEADY"] = Num(p.Steady),
        ["VN_AMBIENT"] = Num(p.Ambient),
        ["VN_SKIP_BVH"] = p.SkipBvh ? "1" : "0",
        ["VN_SKIP_HAIRSIM"] = p.SkipHairSim ? "1" : "0",
        ["VN_LEAVE_OUT"] = p.LeaveOut ? "1" : "0",
        ["VN_DRED"] = p.Dred switch { DredMode.Light => "light", DredMode.Full => "1", _ => "0" },
    };

    public async Task<ScriptPreset> ResolveAsync(LaunchProfile p, RunMode mode, CancellationToken ct = default)
    {
        var psi = new ProcessStartInfo("powershell.exe")
        {
            UseShellExecute = false,
            RedirectStandardOutput = true,
            RedirectStandardError = true,
            CreateNoWindow = true,
            StandardOutputEncoding = Encoding.UTF8,
            WorkingDirectory = repo.Root,
        };
        foreach (var a in new[] { "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", repo.Script }) psi.ArgumentList.Add(a);
        foreach (var a in ScriptArgs(p, mode)) psi.ArgumentList.Add(a);
        foreach (var (k, v) in ScriptVariables(p)) psi.Environment[k] = v;

        using var proc = Process.Start(psi) ?? throw new PlanException("powershell.exe did not start.");
        var stdout = proc.StandardOutput.ReadToEndAsync(ct);
        var stderr = proc.StandardError.ReadToEndAsync(ct);
        await proc.WaitForExitAsync(ct).ConfigureAwait(false);
        var output = (await stdout.ConfigureAwait(false)).Trim();
        var errors = (await stderr.ConfigureAwait(false)).Trim();
        return ParsePreset(output, errors);
    }

    internal static ScriptPreset ParsePreset(string output, string errors)
    {
        var last = output.Split('\n').Select(l => l.Trim()).LastOrDefault(l => l.Length > 0) ?? "";
        if (!last.StartsWith('{'))
        {
            var why = string.Join(" ", new[] { output, errors }.Where(s => s.Length > 0));
            throw new PlanException(why.Length > 0 ? $"play-nvidia.ps1: {why}" : "play-nvidia.ps1 printed nothing.");
        }
        ScriptPreset? preset;
        try { preset = JsonSerializer.Deserialize<ScriptPreset>(last, new JsonSerializerOptions { PropertyNameCaseInsensitive = true }); }
        catch (JsonException e) { throw new PlanException($"play-nvidia.ps1 printed settings that are not JSON: {e.Message}"); }
        if (preset == null || preset.Args.Count == 0 || preset.Settings.Count == 0)
            throw new PlanException("play-nvidia.ps1 printed no settings. Is it a version without -PrintEnv?");
        return preset;
    }

    /// <summary>The overrides of a profile, as the layer reads them.</summary>
    public static Dictionary<string, string> Overrides(LaunchProfile p)
    {
        var o = new Dictionary<string, string>();
        void Int(string k, int? v) { if (v is int i) o[k] = i.ToString(CultureInfo.InvariantCulture); }
        void Bool(string k, bool? v) { if (v is bool b) o[k] = b ? "1" : "0"; }
        Int("VICTORY_PIPELINE_WAIT_MS", p.PipelineWaitMs);
        Int("VICTORY_TEXTURE_MIP_DROP", p.MipDrop);
        Int("VICTORY_TEXTURE_MIP_DROP_MIN_SIZE", p.MipDropMinSize);
        Int("VICTORY_WIDE_THREADS", p.WideThreads);
        Int("VICTORY_CARD_ROOM_FOR_TARGETS_MB", p.TargetRoomMb);
        Bool("VICTORY_POOL_DISCARD", p.PoolDiscard);
        Bool("VICTORY_PIPELINE_BOOK", p.PipelineBook);
        if (p.D3D12BackgroundOff) o["VICTORY_D3D12_BACKGROUND"] = "0";
        if (p.Panel) o["VICTORY_PANEL"] = "1";
        o["VICTORY_LOG"] = p.LogFilter.Trim();
        return o;
    }

    public async Task<LaunchPlan> BuildAsync(LaunchProfile p, RunMode mode, CancellationToken ct = default)
    {
        var blocking = ProfileValidator.Validate(p, repo).Where(i => i.Blocking).ToList();
        if (blocking.Count > 0) throw new PlanException(string.Join(" ", blocking.Select(i => i.Message)));
        var preset = await ResolveAsync(p, mode, ct).ConfigureAwait(false);
        return Compose(p, mode, preset, DateTime.Now);
    }

    public LaunchPlan Compose(LaunchProfile p, RunMode mode, ScriptPreset preset, DateTime now)
    {
        var settings = new Dictionary<string, string>(preset.Settings);
        var overrides = Overrides(p);
        var changed = new List<string>();
        foreach (var (k, v) in overrides)
        {
            if (!settings.TryGetValue(k, out var was) || was != v) changed.Add(k);
            settings[k] = v;
        }
        var cache = string.IsNullOrWhiteSpace(p.CacheDir) ? AppPaths.VictoryDefault("cache") : p.CacheDir.Trim();
        var data = string.IsNullOrWhiteSpace(p.DataDir) ? AppPaths.VictoryDefault("data") : p.DataDir.Trim();
        settings["VICTORY_CACHE_DIR"] = cache;
        settings["VICTORY_DATA_DIR"] = data;
        settings["VICTORY_GAME_DIR"] = Path.GetDirectoryName(Path.GetFullPath(p.GameExe))!;
        // tracing-subscriber writes colour codes unless told not to; the log is a file and a list here.
        settings["NO_COLOR"] = "1";

        var stamp = now.ToString("yyyyMMdd-HHmmss", CultureInfo.InvariantCulture);
        var name = $"{stamp}-{mode.ToString().ToLowerInvariant()}";
        string? frameLog = null;
        if (mode == RunMode.Play)
        {
            frameLog = Path.Combine(AppPaths.Logs, name + ".frames.txt");
            settings["VICTORY_FRAME_LOG"] = frameLog;
        }

        var victory = preset.Victory;
        if (preset.SmoothMotion) victory = Path.Combine(Path.GetDirectoryName(victory)!, "victory_sm.exe");
        return new LaunchPlan
        {
            Mode = mode,
            Victory = victory,
            WorkDir = string.IsNullOrEmpty(preset.Workdir) ? repo.Root : preset.Workdir,
            Args = preset.Args,
            Settings = settings,
            Overridden = changed,
            LogPath = Path.Combine(AppPaths.Logs, name + ".log"),
            FrameLogPath = frameLog,
            GameExe = p.GameExe,
            DataDir = data,
            CacheDir = cache,
        };
    }

    private static string Num(double v) => v.ToString("0.###", CultureInfo.InvariantCulture);
}
