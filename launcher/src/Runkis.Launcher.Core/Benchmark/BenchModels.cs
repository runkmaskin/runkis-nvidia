using System.Text.RegularExpressions;

namespace Runkis.Launcher.Core;

public enum CandidateKind { Preset, Profile }

/// <summary>One way to start that a benchmark measures: a preset or saved profile, plus raw layer switches on top.</summary>
public sealed partial class BenchCandidate
{
    public string Name { get; set; } = "";
    public CandidateKind Kind { get; set; }
    /// <summary>The preset's id or the profile's name it was made from.</summary>
    public string Source { get; set; } = "";
    /// <summary>The profile the run starts with (a preset applied over the user's profile), fixed when the candidate was added.</summary>
    public LaunchProfile Profile { get; set; } = new();
    /// <summary><c>VICTORY_X=Y</c> settings put over the plan's, for A/B of the layer's own switches.</summary>
    public Dictionary<string, string> Extra { get; set; } = [];

    /// <summary>Settings a benchmark sets itself; a candidate may not.</summary>
    public static readonly IReadOnlySet<string> Reserved = new HashSet<string>(StringComparer.Ordinal) { "VICTORY_FRAME_LOG", KeyScript.Variable };

    [GeneratedRegex(@"^VICTORY_[A-Z0-9_]+=.*$", RegexOptions.CultureInvariant)]
    private static partial Regex ExtraLine();

    /// <summary>
    /// Raw override lines, one <c>VICTORY_X=Y</c> each (blank lines ignored). Errors name the lines that are not.
    /// </summary>
    public static Dictionary<string, string> ParseExtra(string? text, out List<string> errors)
    {
        errors = [];
        var extra = new Dictionary<string, string>(StringComparer.Ordinal);
        var n = 0;
        foreach (var raw in (text ?? "").Replace("\r", "", StringComparison.Ordinal).Split('\n'))
        {
            n++;
            var line = raw.Trim();
            if (line.Length == 0) continue;
            if (!ExtraLine().IsMatch(line)) { errors.Add($"Line {n}: '{line}' is not VICTORY_NAME=value (capitals, digits and _ only)."); continue; }
            var eq = line.IndexOf('=', StringComparison.Ordinal);
            var key = line[..eq];
            if (Reserved.Contains(key)) { errors.Add($"Line {n}: {key} is set by the benchmark itself{(key == KeyScript.Variable ? "; use the key script" : "")}."); continue; }
            if (extra.ContainsKey(key)) { errors.Add($"Line {n}: {key} is given twice."); continue; }
            extra[key] = line[(eq + 1)..];
        }
        return extra;
    }

    public string ExtraText => string.Join(" ", Extra.Select(kv => $"{kv.Key}={kv.Value}"));
}

public sealed class BenchOptions
{
    public double WarmupSeconds { get; set; } = 40;
    public double MeasureSeconds { get; set; } = 90;
    public int Repeats { get; set; } = 2;
    /// <summary>The VICTORY_KEYS value (already normalised), or empty for none.</summary>
    public string Keys { get; set; } = "";
    /// <summary>A run that shows no picture within warm-up + this many seconds of its start has failed.</summary>
    public double FirstFrameGraceSeconds { get; set; } = 120;
    /// <summary>How long the game gets to close its window before it is ended (EmulatorSession.StopAsync).</summary>
    public double StopGraceSeconds { get; set; } = 8;
    /// <summary>A run with no picture for this long at the end of the measured window froze: it has failed.</summary>
    public double FrozenSeconds { get; set; } = 10;

    public List<string> Validate()
    {
        var e = new List<string>();
        if (!(WarmupSeconds >= 0 && WarmupSeconds <= 3600)) e.Add("The warm-up must be 0 to 3600 s.");
        if (!(MeasureSeconds >= 5 && MeasureSeconds <= 3600)) e.Add("The measured time must be 5 to 3600 s.");
        if (Repeats is < 1 or > 20) e.Add("Repeats must be 1 to 20.");
        return e;
    }
}

public enum RunStatus { Ok, Failed, Cancelled }

/// <summary>What one run measured. Null: not measured (the source had nothing in the window), never a made-up zero.</summary>
public sealed class RunMetrics
{
    public double? AvgFps { get; set; }
    public double? Low1Fps { get; set; }
    public double? MedianMs { get; set; }
    public double? WorstMs { get; set; }
    public int? Hitches { get; set; }
    public int Frames { get; set; }
    public double? PreparingMs { get; set; }
    public double? GpuMs { get; set; }
    public double? CopyBackMs { get; set; }
    public long? PeakPrivateMb { get; set; }
    public long? PeakVramMb { get; set; }
    /// <summary>How many five-second lines fell inside the window.</summary>
    public int FpsLines { get; set; }
    public int MemoryLines { get; set; }
}

public sealed class BenchRun
{
    /// <summary>Position in the run order, from 0.</summary>
    public int Order { get; set; }
    public int Candidate { get; set; }
    public int Repeat { get; set; }
    public RunStatus Status { get; set; }
    public string? Failure { get; set; }
    public List<string> Problems { get; set; } = [];
    public RunMetrics Metrics { get; set; } = new();
    /// <summary>Seconds from the process start to the first picture (the frame log's 0).</summary>
    public double? FirstFrameSeconds { get; set; }
    /// <summary>Times the title restarted itself (the frame log started again; timing restarts with it).</summary>
    public int Restarts { get; set; }
    public string? LogPath { get; set; }
    public string? FrameLogPath { get; set; }
    public DateTime Started { get; set; }
    public DateTime Ended { get; set; }
}

public sealed class BenchResult
{
    public int SchemaVersion { get; set; } = 1;
    public DateTime Started { get; set; }
    public DateTime Finished { get; set; }
    public BenchOptions Options { get; set; } = new();
    public List<BenchCandidate> Candidates { get; set; } = [];
    public List<BenchRun> Runs { get; set; } = [];
    public bool Cancelled { get; set; }
    /// <summary>The PC it ran on, for reading old results.</summary>
    public string Machine { get; set; } = "";
    /// <summary>Where it was saved (not stored in the file).</summary>
    [System.Text.Json.Serialization.JsonIgnore]
    public string? Path { get; set; }
}

/// <summary>The order runs go in: the candidates interleaved, A B A B, so drift over the session averages out.</summary>
public static class BenchOrder
{
    public static List<(int Candidate, int Repeat)> Interleave(int candidates, int repeats)
    {
        var order = new List<(int, int)>();
        for (var r = 0; r < repeats; r++)
            for (var c = 0; c < candidates; c++) order.Add((c, r));
        return order;
    }
}
