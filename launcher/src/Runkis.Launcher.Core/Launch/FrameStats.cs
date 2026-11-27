using System.Globalization;
using System.Text.RegularExpressions;

namespace Runkis.Launcher.Core;

/// <summary>One presented frame, as VICTORY_FRAME_LOG writes it: seconds since start, ms since the last present, draws.</summary>
public readonly record struct FrameSample(double At, double Ms, long Draws);

/// <summary>Frame-pacing figures over the frames of the last few seconds. Null means "not known yet", never zero.</summary>
public sealed record FrameSummary(double? Fps, double? AvgMs, double? Low1Fps, double? WorstMs, int Hitches, int Frames);

/// <summary>The frames the layer wrote down, kept for a while, and figures over them.</summary>
public sealed class FrameStats
{
    private readonly object gate = new();
    private readonly Queue<FrameSample> frames = new();
    private readonly double keepSeconds;
    public const double HitchMs = 100;

    public FrameStats(double keepSeconds = 60) => this.keepSeconds = keepSeconds;

    /// <summary>Parses one frame-log line; false for anything else. The first three columns are read; the layer may
    /// write more after them (docs/frame-pacing.md: the waits of the frame), which are left to other readers.</summary>
    public static bool TryParse(string line, out FrameSample sample)
    {
        sample = default;
        var parts = line.Split(' ', StringSplitOptions.RemoveEmptyEntries);
        if (parts.Length < 3) return false;
        if (!double.TryParse(parts[0], NumberStyles.Float, CultureInfo.InvariantCulture, out var at)) return false;
        if (!double.TryParse(parts[1], NumberStyles.Float, CultureInfo.InvariantCulture, out var ms)) return false;
        if (!long.TryParse(parts[2], NumberStyles.Integer, CultureInfo.InvariantCulture, out var draws)) return false;
        sample = new FrameSample(at, ms, draws);
        return ms >= 0;
    }

    public void Add(FrameSample s)
    {
        lock (gate)
        {
            frames.Enqueue(s);
            while (frames.Count > 0 && s.At - frames.Peek().At > keepSeconds) frames.Dequeue();
        }
    }

    public FrameSample[] Recent(int max)
    {
        lock (gate) return frames.Skip(Math.Max(0, frames.Count - max)).ToArray();
    }

    /// <summary>Figures over the last <paramref name="seconds"/> of frames.</summary>
    public FrameSummary Summary(double seconds = 10)
    {
        FrameSample[] window;
        lock (gate)
        {
            if (frames.Count == 0) return new(null, null, null, null, 0, 0);
            var end = frames.Last().At;
            window = frames.Where(f => end - f.At <= seconds).ToArray();
        }
        // (The log's first line has a gap of about 0: the log opens at the first present.)
        // 1 % low: the frame rate of the slowest 1 % of frames (at least one), averaged. A percentile would step over
        // the few long frames that make a stutter, which are what this figure is for. The benchmark uses the same.
        var f = FramePacing.Of(window.Select(s => s.Ms));
        return new(f.AvgFps, f.AvgMs, f.Low1Fps, f.WorstMs, f.Hitches ?? 0, f.Frames);
    }

    public void Clear() { lock (gate) frames.Clear(); }
}

/// <summary>Reads new lines of a file another process is writing (shared read, no lock on it).</summary>
public sealed class FileTail(string path)
{
    private long offset;
    private string partial = "";

    public string Path { get; } = path;

    public List<string> ReadNew()
    {
        var lines = new List<string>();
        if (!File.Exists(Path)) return lines;
        using var fs = new FileStream(Path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete);
        if (fs.Length < offset) { offset = 0; partial = ""; }  // the file was made again
        fs.Seek(offset, SeekOrigin.Begin);
        using var reader = new StreamReader(fs);
        var text = partial + reader.ReadToEnd();
        offset = fs.Length;
        var parts = text.Split('\n');
        partial = parts[^1];
        for (var i = 0; i < parts.Length - 1; i++) lines.Add(parts[i].TrimEnd('\r'));
        return lines;
    }
}

/// <summary>The window caption the layer appends about once a second (victory-gpu host.rs Meter::frame).</summary>
public static partial class Caption
{
    [GeneratedRegex(@"\| VIctory (?<fps>\d+) fps (?<ms>[\d.]+) ms/frame (?<draws>\d+) draws/frame")]
    private static partial Regex Pattern();

    public sealed record Reading(int Fps, double Ms, int Draws);

    public static Reading? Parse(string caption)
    {
        var m = Pattern().Match(caption);
        if (!m.Success) return null;
        return new(int.Parse(m.Groups["fps"].Value, CultureInfo.InvariantCulture),
            double.Parse(m.Groups["ms"].Value, CultureInfo.InvariantCulture),
            int.Parse(m.Groups["draws"].Value, CultureInfo.InvariantCulture));
    }

    /// <summary>The newest reading from the windows of a process, if any shows one.</summary>
    public static Reading? Read(int pid)
    {
        foreach (var h in Native.WindowsOf(pid))
            if (Parse(Native.WindowText(h)) is { } r) return r;
        return null;
    }
}
