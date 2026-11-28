using System.Globalization;
using System.Text.RegularExpressions;

namespace Runkis.Launcher.Core;

/// <summary>
/// The frame-rate line the layer logs every five seconds (victory-gpu host.rs, <c>Meter::frame</c>): its figures are
/// per frame, over the second before the line.
/// </summary>
public sealed record FpsLine(double Fps, double MsPerFrame, double PreparingMs, double CopiesMs, double CopiesMb,
    double TexturesMs, double TexturesMb, double GpuMs, double CopyBackMs, double CopyBackMb);

/// <summary>Parsers for the layer's own stderr lines. Each one is checked by a test against the format string in host.rs.</summary>
public static partial class LayerLines
{
    // "{fps:.1} fps, {ms:.1} ms/frame ({cpu:.1} preparing: copies {copy_ms:.1} ms {copy_mb:.1} MB, textures {texture_ms:.1} ms
    //  {texture_mb:.1} MB; {gpu:.1} on the GPU; {back_ms:.1} copying back {back_mb:.1} MB), {draws:.0} draws/frame, ..."
    [GeneratedRegex(@"(?<![\d.])(?<fps>\d+(?:\.\d+)?) fps, (?<ms>\d+(?:\.\d+)?) ms/frame \((?<cpu>\d+(?:\.\d+)?) preparing: copies (?<cms>\d+(?:\.\d+)?) ms (?<cmb>\d+(?:\.\d+)?) MB, textures (?<tms>\d+(?:\.\d+)?) ms (?<tmb>\d+(?:\.\d+)?) MB; (?<gpu>\d+(?:\.\d+)?) on the GPU; (?<bms>\d+(?:\.\d+)?) copying back (?<bmb>\d+(?:\.\d+)?) MB\)",
        RegexOptions.CultureInvariant)]
    private static partial Regex FpsPattern();

    /// <summary>The prefix of host.rs <c>memory_text</c>.</summary>
    public const string MemoryPrefix = "memory by subsystem:";

    public static FpsLine? ParseFps(string line)
    {
        if (!line.Contains(" ms/frame (", StringComparison.Ordinal)) return null;
        var m = FpsPattern().Match(line);
        if (!m.Success) return null;
        double G(string n) => double.Parse(m.Groups[n].Value, NumberStyles.Float, CultureInfo.InvariantCulture);
        return new(G("fps"), G("ms"), G("cpu"), G("cms"), G("cmb"), G("tms"), G("tmb"), G("gpu"), G("bms"), G("bmb"));
    }

    /// <summary>
    /// The memory line: <c>memory by subsystem:</c>, then <c>name=MB name_peak=MB</c> for each field, separated by spaces.
    /// Null when the line is not one; fields that are not <c>name=whole number</c> are left out.
    /// </summary>
    public static IReadOnlyDictionary<string, long>? ParseMemory(string line)
    {
        var at = line.IndexOf(MemoryPrefix, StringComparison.Ordinal);
        if (at < 0) return null;
        var fields = new Dictionary<string, long>(StringComparer.Ordinal);
        foreach (var part in line[(at + MemoryPrefix.Length)..].Split(' ', StringSplitOptions.RemoveEmptyEntries))
        {
            var eq = part.IndexOf('=', StringComparison.Ordinal);
            if (eq <= 0) continue;
            if (long.TryParse(part[(eq + 1)..], NumberStyles.None, CultureInfo.InvariantCulture, out var mb)) fields[part[..eq]] = mb;
        }
        return fields.Count > 0 ? fields : null;
    }

    /// <summary>The process's private bytes now, in MB (host.rs field <c>private</c>).</summary>
    public const string PrivateField = "private";
    /// <summary>The video memory the system counts for the process now, in MB (host.rs field <c>gpu_used</c>).</summary>
    public const string VramField = "gpu_used";
}

/// <summary>Frame-pacing figures over a set of frames. Null: not known (fewer than two frames), never zero.</summary>
public sealed record PacingFigures(double? AvgFps, double? AvgMs, double? MedianMs, double? Low1Fps, double? WorstMs, int? Hitches, int Frames);

public static class FramePacing
{
    public const double HitchMs = 100;

    /// <summary>
    /// The frames presented inside the measured window: frame-log seconds after <paramref name="from"/>, up to and
    /// including <paramref name="to"/>. (A frame's line carries the time it was presented and the gap before it.)
    /// </summary>
    public static List<FrameSample> Window(IEnumerable<FrameSample> frames, double from, double to) =>
        frames.Where(f => f.At > from && f.At <= to).ToList();

    /// <summary>
    /// Average fps (frames over their total time), median frame time, 1 % low (the frame rate of the slowest 1 % of
    /// frames, at least one, averaged, as <see cref="FrameStats"/>), worst frame and hitches of 100 ms or more.
    /// Gaps of 0.01 ms or less are left out: the log's first line has a gap of about 0.
    /// </summary>
    public static PacingFigures Of(IEnumerable<double> frameMs)
    {
        var sorted = frameMs.Where(ms => ms > 0.01).Order().ToArray();
        if (sorted.Length < 2) return new(null, null, null, null, null, null, sorted.Length);
        var total = sorted.Sum();
        var n = sorted.Length;
        var median = n % 2 == 1 ? sorted[n / 2] : (sorted[n / 2 - 1] + sorted[n / 2]) / 2;
        var slowest = sorted.TakeLast(Math.Max(1, n / 100)).Average();
        return new(1000.0 * n / total, total / n, median, slowest > 0 ? 1000.0 / slowest : null, sorted[^1],
            sorted.Count(t => t >= HitchMs), n);
    }
}
