using System.Globalization;
using System.Text.RegularExpressions;

namespace Runkis.Launcher.Core;

/// <summary>
/// How far a warm-up has got, read from victory.exe's own progress lines. Building pipelines (`victory warm`) states
/// its total first; translating shaders (`victory shaders`) does not know it before the end, so the total of the
/// last translation is used, if there was one.
/// </summary>
public sealed partial class WarmProgress
{
    // victory warm: "83564 pipelines known, 0 made before (...), 0 bad; 83564 to make, 10 processes"
    [GeneratedRegex(@"^(\d+) pipelines known, .*; (\d+) to make,")]
    private static partial Regex WarmTotal();
    // victory warm: "19000 of 83564, 474 s"
    [GeneratedRegex(@"^(\d+) of (\d+), (\d+) s$")]
    private static partial Regex WarmStep();
    // victory shaders: "12000 shaders in the cache, 95 s" and at the end "40211 shaders in the cache, 7 do not translate, 310 s"
    [GeneratedRegex(@"^(\d+) shaders in the cache, (?:(\d+) do not translate, )?(\d+) s$")]
    private static partial Regex ShaderStep();

    private readonly List<(double Seconds, long Done)> samples = [];

    public long Done { get; private set; }
    /// <summary>0 when not known.</summary>
    public long Total { get; private set; }
    public double Seconds { get; private set; }
    /// <summary>The translation reported its last line: its count is the total for the next one.</summary>
    public long? FinishedTotal { get; private set; }

    public WarmProgress(long knownTotal = 0) => Total = knownTotal;

    /// <summary>Takes one output line; true if it moved the progress.</summary>
    public bool Take(string line)
    {
        var t = line.Trim();
        if (WarmTotal().Match(t) is { Success: true } total)
        {
            Total = long.Parse(total.Groups[2].Value, CultureInfo.InvariantCulture);
            Done = 0;
            return true;
        }
        if (WarmStep().Match(t) is { Success: true } step)
        {
            Total = long.Parse(step.Groups[2].Value, CultureInfo.InvariantCulture);
            Note(long.Parse(step.Groups[1].Value, CultureInfo.InvariantCulture), double.Parse(step.Groups[3].Value, CultureInfo.InvariantCulture));
            return true;
        }
        if (ShaderStep().Match(t) is { Success: true } shaders)
        {
            var done = long.Parse(shaders.Groups[1].Value, CultureInfo.InvariantCulture);
            if (shaders.Groups[2].Success)
            {
                // The last line: what did not translate is part of what was looked at.
                FinishedTotal = done + long.Parse(shaders.Groups[2].Value, CultureInfo.InvariantCulture);
                Total = FinishedTotal.Value;
                done = Total;
            }
            else if (done > Total)
                Total = 0; // more than the last translation had: a different game build, the old total is no use
            Note(done, double.Parse(shaders.Groups[3].Value, CultureInfo.InvariantCulture));
            return true;
        }
        return false;
    }

    private void Note(long done, double seconds)
    {
        Done = done;
        Seconds = seconds;
        samples.Add((seconds, done));
    }

    /// <summary>0..100, or null when the total is not known.</summary>
    public double? Percent => Total > 0 ? Math.Min(100.0, 100.0 * Done / Total) : null;

    /// <summary>
    /// Time left at the rate of the last five minutes (pipelines get slower towards the end of the list, so the
    /// whole run's rate would promise too little); null until two samples a minute apart.
    /// </summary>
    public TimeSpan? Left
    {
        get
        {
            if (Total <= 0 || samples.Count < 2) return null;
            var last = samples[^1];
            var first = samples.FirstOrDefault(s => last.Seconds - s.Seconds <= 300);
            if (last.Seconds - first.Seconds < 60 || last.Done <= first.Done) return null;
            var rate = (last.Done - first.Done) / (last.Seconds - first.Seconds);
            return TimeSpan.FromSeconds(Math.Max(0, Total - last.Done) / rate);
        }
    }

    /// <summary>"19 000 of 83 564 (23 %) · about 45 min left", or the count alone when the total is not known.</summary>
    public string Describe(string what)
    {
        var n = CultureInfo.InvariantCulture;
        string Num(long v) => v.ToString("#,0", n).Replace(",", " ");
        if (Percent is not double p) return $"{Num(Done)} {what}";
        var text = $"{Num(Done)} of {Num(Total)} {what} ({p.ToString("0", n)} %)";
        if (Left is TimeSpan left)
            text += left.TotalMinutes >= 1 ? $" · about {Math.Ceiling(left.TotalMinutes).ToString(n)} min left" : " · under a minute left";
        return text;
    }
}
