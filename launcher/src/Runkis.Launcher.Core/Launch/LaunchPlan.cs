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
    /// <summary>
    /// The command and its options: for Play, <c>play &lt;exe&gt; [options]</c> without the <c>--set</c>s, which
    /// <see cref="Args"/> adds from <see cref="Sets"/>.
    /// </summary>
    public required IReadOnlyList<string> BaseArgs { get; init; }
    /// <summary>
    /// The switches the profile (and a benchmark) sets over `victory play`'s own. A play run gets them as <c>--set</c>:
    /// `victory play` clears every other VICTORY_* variable, and its restarts (the first start's warm-up, the F1 menu)
    /// start it again with the same options.
    /// </summary>
    public required IReadOnlyDictionary<string, string> Sets { get; init; }
    /// <summary>The settings the run gets, as `victory play --print-env` reported them with <see cref="Sets"/>, and the paths.</summary>
    public required IReadOnlyDictionary<string, string> Settings { get; init; }
    /// <summary>The settings that came from the profile on top of `victory play`'s, for display.</summary>
    public required IReadOnlyCollection<string> Overridden { get; init; }
    public required string LogPath { get; init; }
    public string? FrameLogPath { get; init; }
    public required string GameExe { get; init; }
    public required string DataDir { get; init; }
    public required string CacheDir { get; init; }

    public IReadOnlyList<string> Args => Mode == RunMode.Play
        ? [.. BaseArgs, .. Sets.OrderBy(kv => kv.Key, StringComparer.Ordinal).SelectMany(kv => new[] { "--set", $"{kv.Key}={kv.Value}" })]
        : BaseArgs;

    public string CommandLine => Quote(Victory) + " " + string.Join(" ", Args.Select(Quote));

    /// <summary>The same plan with <paramref name="extra"/> set over its settings, and its logs at <paramref name="logPath"/>.</summary>
    public LaunchPlan With(IReadOnlyDictionary<string, string> extra, string logPath, string? frameLogPath)
    {
        var settings = new Dictionary<string, string>(Settings);
        var sets = new Dictionary<string, string>(Sets);
        var overridden = Overridden.ToList();
        foreach (var (k, v) in extra)
        {
            if (!settings.TryGetValue(k, out var was) || was != v) overridden.Add(k);
            settings[k] = v;
            sets[k] = v;
        }
        if (frameLogPath != null) settings["VICTORY_FRAME_LOG"] = sets["VICTORY_FRAME_LOG"] = frameLogPath;
        return new LaunchPlan
        {
            Mode = Mode, Victory = Victory, WorkDir = WorkDir, BaseArgs = BaseArgs, Sets = sets, Settings = settings,
            Overridden = overridden.Distinct().ToList(), LogPath = logPath, FrameLogPath = frameLogPath, GameExe = GameExe,
            DataDir = DataDir, CacheDir = CacheDir,
        };
    }

    private static string Quote(string a) => a.Contains(' ') ? $"\"{a}\"" : a;
}

/// <summary>What `victory play --print-env` answers (apps/victory/src/play.rs, <c>env_json</c>).</summary>
public sealed class ScriptPreset
{
    public string Victory { get; set; } = "";
    public string Exe { get; set; } = "";
    public string Console { get; set; } = "";
    /// <summary>The `run` a start does: <c>run &lt;exe&gt; --console &lt;console&gt; --execute</c>.</summary>
    public List<string> Args { get; set; } = [];
    public Dictionary<string, string> Settings { get; set; } = [];
}

public sealed class PlanException(string message) : Exception(message);

/// <summary>
/// Builds a <see cref="LaunchPlan"/>: `victory play` decides the base settings (one source of truth with a double
/// click on victory.exe and with scripts\play-nvidia.ps1), the profile's switches go on top as <c>--set</c>, and the
/// paths and logs are the launcher's.
/// </summary>
public sealed class PlanBuilder(RepoLayout repo)
{
    /// <summary>`victory play`'s options for a profile (without the <c>--set</c>s).</summary>
    public static List<string> PlayArgs(LaunchProfile p)
    {
        var a = new List<string> { "play", p.GameExe };
        if (p.Console == ConsoleModel.SeriesS) a.Add("--series-s");
        a.AddRange(["--res", p.Picture switch { SeriesXPicture.P1080To1440 => "1080to1440", SeriesXPicture.P4k => "4k", _ => "1440" }]);
        a.AddRange(["--dlss", (p.Dlss ?? DlssMode.Quality).ToString().ToLowerInvariant()]);
        if (p.FrameGen is >= 2 and <= 6) a.AddRange(["--framegen", p.FrameGen.ToString(CultureInfo.InvariantCulture)]);
        a.AddRange(["--fg-type", p.FgType.ToString().ToLowerInvariant()]);
        a.AddRange(["--fps", Math.Clamp(p.Fps, 1, 1000).ToString(CultureInfo.InvariantCulture)]);
        return a;
    }

    /// <summary>The command a mode runs: play, or the warm-up's steps with play's settings in the environment.</summary>
    public static List<string> ModeArgs(LaunchProfile p, RunMode mode) => mode switch
    {
        RunMode.Warm => ["warm", .. p.WarmAll ? Array.Empty<string>() : ["--used"], .. p.WarmLimit > 0 ? ["--limit", p.WarmLimit.ToString(CultureInfo.InvariantCulture)] : Array.Empty<string>()],
        RunMode.Precompile => ["shaders", Path.GetDirectoryName(Path.GetFullPath(p.GameExe))!],
        _ => PlayArgs(p),
    };

    /// <summary>The switches a profile sets over `victory play`'s, as the layer reads them; nothing for a choice left as the layer has it.</summary>
    public static Dictionary<string, string> Overrides(LaunchProfile p)
    {
        var o = new Dictionary<string, string>();
        void Int(string k, int? v) { if (v is int i) o[k] = i.ToString(CultureInfo.InvariantCulture); }
        void Bool(string k, bool? v) { if (v is bool b) o[k] = b ? "1" : "0"; }
        // This fork's switches on upstream's base (both off unless the profile asks).
        if (p.FenceSleep) o["VICTORY_FENCE_SLEEP"] = "1";
        if (!p.WaterLight) o["VICTORY_WATER_LIGHT"] = "0";
        if (p.TitlePool is > 0 and <= 16) Int("VICTORY_TITLE_POOL", p.TitlePool);
        Int("VICTORY_TITLE_PRIORITY_CAP", p.TitlePriorityCap);
        // Upstream's own switches the launcher has settings for.
        if (p.Fullscreen == true) o["VICTORY_FULLSCREEN"] = "1";
        // The mouse (victory-hle gameinput.rs): the stand-in pad's right stick (this fork's VICTORY_MOUSE, the launcher's
        // default: the game's own mouse did not turn the camera), or upstream's native mouse with the cursor held.
        if (p.Mouse) o["VICTORY_MOUSE"] = "1";
        if (p.MouseCapture) o["VICTORY_MOUSE_CAPTURE"] = "1";
        if ((p.Mouse || p.MouseCapture) && Math.Abs(p.MouseSense - 1.0) > 1e-9) o["VICTORY_MOUSE_SENSE"] = Num(p.MouseSense);
        if ((p.Mouse || p.MouseCapture) && p.MouseInvertY) o["VICTORY_MOUSE_INVERT_Y"] = "1";
        // The layer's own defaults need no list, so an unchanged map leaves its mapping exactly.
        if (KeyMap.Variable(p) is string keys) o["VICTORY_KEYMAP"] = keys;
        if (p.Panel) o["VICTORY_PANEL"] = "1";
        if (!string.IsNullOrWhiteSpace(p.Adapter)) o["VICTORY_ADAPTER"] = p.Adapter.Trim();
        Int("VICTORY_PIPELINE_WAIT_MS", p.PipelineWaitMs);
        Int("VICTORY_WIDE_THREADS", p.WideThreads);
        Int("VICTORY_CARD_ROOM_FOR_TARGETS_MB", p.TargetRoomMb);
        Bool("VICTORY_PIPELINE_BOOK", p.PipelineBook);
        if (p.D3D12BackgroundOff) o["VICTORY_D3D12_BACKGROUND"] = "0";
        if (p.Dred != DredMode.Off) o["VICTORY_D3D12_DRED"] = p.Dred == DredMode.Light ? "light" : "1";
        return o;
    }

    private static string Num(double v) => v.ToString("0.###", CultureInfo.InvariantCulture);

    /// <summary>The environment a run gets beside its settings: the paths (kept by `victory play`) and the log filter.</summary>
    public static Dictionary<string, string> Paths(LaunchProfile p)
    {
        var cache = string.IsNullOrWhiteSpace(p.CacheDir) ? AppPaths.VictoryDefault("cache") : p.CacheDir.Trim();
        var data = string.IsNullOrWhiteSpace(p.DataDir) ? AppPaths.VictoryDefault("data") : p.DataDir.Trim();
        return new()
        {
            ["VICTORY_CACHE_DIR"] = cache,
            ["VICTORY_DATA_DIR"] = data,
            ["VICTORY_GAME_DIR"] = Path.GetDirectoryName(Path.GetFullPath(p.GameExe))!,
            ["VICTORY_LOG"] = string.IsNullOrWhiteSpace(p.LogFilter) ? "info" : p.LogFilter.Trim(),
            // tracing-subscriber writes colour codes unless told not to; the log is a file and a list here.
            ["NO_COLOR"] = "1",
        };
    }

    /// <summary>Asks `victory play --print-env` for the settings of the profile's start with its switches.</summary>
    public async Task<ScriptPreset> ResolveAsync(LaunchProfile p, RunMode mode, CancellationToken ct = default)
    {
        var victory = repo.VictoryExe(p.Build);
        if (!File.Exists(victory)) throw new PlanException($"{victory} is not built. Build it with: {RepoLayout.BuildCommand(p.Build)}");
        var psi = new ProcessStartInfo(victory)
        {
            UseShellExecute = false,
            RedirectStandardOutput = true,
            RedirectStandardError = true,
            RedirectStandardInput = true,
            CreateNoWindow = true,
            StandardOutputEncoding = Encoding.UTF8,
            WorkingDirectory = repo.Root,
        };
        foreach (var a in PlayArgs(p)) psi.ArgumentList.Add(a);
        foreach (var (k, v) in Overrides(p)) { psi.ArgumentList.Add("--set"); psi.ArgumentList.Add($"{k}={v}"); }
        psi.ArgumentList.Add("--print-env");
        foreach (var key in psi.Environment.Keys.Where(k => k.StartsWith("VICTORY_", StringComparison.OrdinalIgnoreCase)).ToList())
            psi.Environment.Remove(key);
        foreach (var (k, v) in Paths(p)) psi.Environment[k] = v;

        using var proc = Process.Start(psi) ?? throw new PlanException($"{victory} did not start.");
        proc.StandardInput.Close();
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
            throw new PlanException(why.Length > 0 ? $"victory play --print-env: {why}" : "victory play --print-env printed nothing.");
        }
        ScriptPreset? preset;
        try { preset = JsonSerializer.Deserialize<ScriptPreset>(last, new JsonSerializerOptions { PropertyNameCaseInsensitive = true }); }
        catch (JsonException e) { throw new PlanException($"victory play --print-env printed settings that are not JSON: {e.Message}"); }
        if (preset == null || preset.Args.Count == 0 || preset.Settings.Count == 0)
            throw new PlanException("victory play printed no settings. Is it a build without --print-env (older than this launcher)?");
        return preset;
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
        // `victory play` printed its settings with the profile's switches already over its own: those that differ
        // from a plain start are the profile's.
        var settings = new Dictionary<string, string>(preset.Settings);
        var sets = Overrides(p);
        var changed = sets.Keys.ToList();
        var paths = Paths(p);
        foreach (var (k, v) in paths) settings[k] = v;

        var stamp = now.ToString("yyyyMMdd-HHmmss", CultureInfo.InvariantCulture);
        var name = $"{stamp}-{mode.ToString().ToLowerInvariant()}";
        string? frameLog = null;
        if (mode == RunMode.Play)
        {
            frameLog = Path.Combine(AppPaths.Logs, name + ".frames.txt");
            settings["VICTORY_FRAME_LOG"] = sets["VICTORY_FRAME_LOG"] = frameLog;
        }
        return new LaunchPlan
        {
            Mode = mode,
            Victory = string.IsNullOrEmpty(preset.Victory) ? repo.VictoryExe(p.Build) : preset.Victory,
            WorkDir = repo.Root,
            BaseArgs = ModeArgs(p, mode),
            Sets = sets,
            Settings = settings,
            Overridden = changed,
            LogPath = Path.Combine(AppPaths.Logs, name + ".log"),
            FrameLogPath = frameLog,
            GameExe = p.GameExe,
            DataDir = paths["VICTORY_DATA_DIR"],
            CacheDir = paths["VICTORY_CACHE_DIR"],
        };
    }
}
