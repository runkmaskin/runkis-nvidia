using System.Diagnostics;
using System.Globalization;

namespace Runkis.Launcher.Core;

/// <summary>A benchmark run on a real <see cref="EmulatorSession"/>, its frames read from the plan's frame log.</summary>
public sealed class EmulatorBenchSession(EmulatorSession session) : IBenchSession
{
    private readonly FileTail? tail = session.Plan.FrameLogPath is { } f ? new FileTail(f) : null;

    public event Action<LogLine>? Line
    {
        add => session.Line += value;
        remove => session.Line -= value;
    }

    public EmulatorSession Session => session;
    public SessionState State => session.State;
    public string? ExitText => session.ExitText;
    public IReadOnlyCollection<Problem> Problems => session.Problems;
    public string? LogPath => session.Plan.LogPath;
    public string? FrameLogPath => session.Plan.FrameLogPath;

    public void Start() => session.Start();

    public IReadOnlyList<FrameSample> ReadFrames()
    {
        session.Flush();
        if (tail == null) return [];
        try
        {
            var frames = new List<FrameSample>();
            foreach (var line in tail.ReadNew())
                if (FrameStats.TryParse(line, out var s)) frames.Add(s);
            return frames;
        }
        catch (IOException) { return []; }
        catch (UnauthorizedAccessException) { return []; }
    }

    public Task StopAsync(TimeSpan grace) => session.StopAsync(grace);

    public void Dispose() => session.Dispose();

    /// <summary>
    /// The real factory: the launch plan of the candidate's profile through <see cref="PlanBuilder"/> (play-nvidia.ps1,
    /// then the profile's overrides), then the candidate's raw settings and the key script on top, with logs named
    /// <c>&lt;time&gt;-bench.log</c> and <c>.frames.txt</c> in the launcher's log folder.
    /// </summary>
    public static BenchSessionFactory Factory(RepoLayout repo) => async (candidate, keys, ct) =>
    {
        await WaitForNoVictoryAsync(TimeSpan.FromSeconds(15), ct).ConfigureAwait(false);
        var plan = await new PlanBuilder(repo).BuildAsync(candidate.Profile.Clone(), RunMode.Play, ct).ConfigureAwait(false);
        if (!File.Exists(plan.Victory.Replace("victory_sm.exe", "victory.exe", StringComparison.OrdinalIgnoreCase)))
            throw new PlanException($"{plan.Victory} is not built. Build it with: {RepoLayout.BuildCommand(candidate.Profile.Build)}");
        var extra = new Dictionary<string, string>(candidate.Extra);
        if (keys.Length > 0) extra[KeyScript.Variable] = keys;
        var log = Path.Combine(AppPaths.Logs, DateTime.Now.ToString("yyyyMMdd-HHmmss-fff", CultureInfo.InvariantCulture) + "-bench.log");
        return new EmulatorBenchSession(new EmulatorSession(plan.With(extra, log, Path.ChangeExtension(log, ".frames.txt"))));
    };

    /// <summary>One victory process at a time: waits for any other to end, and refuses to start beside one.</summary>
    public static async Task WaitForNoVictoryAsync(TimeSpan patience, CancellationToken ct)
    {
        var until = DateTime.UtcNow + patience;
        while (true)
        {
            var others = Process.GetProcessesByName("victory").Concat(Process.GetProcessesByName("victory_sm")).ToList();
            var pids = others.Select(p => p.Id).ToList();
            foreach (var p in others) p.Dispose();
            if (pids.Count == 0) return;
            if (DateTime.UtcNow >= until)
                throw new PlanException($"Another victory process is running (pid {string.Join(", ", pids)}); a benchmark runs one at a time. Close it and start again.");
            await Task.Delay(500, ct).ConfigureAwait(false);
        }
    }
}
