using System.Globalization;
using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace Runkis.Launcher.Core;

public sealed record BenchEntry(string Path, DateTime Started, string Title, string Summary);

/// <summary>Benchmarks as JSON files named by their start time (%LOCALAPPDATA%\RunkisLauncher\benchmarks\&lt;time&gt;.json).</summary>
public sealed class BenchmarkStore(string folder)
{
    private static readonly JsonSerializerOptions Json = new()
    {
        WriteIndented = true,
        Converters = { new JsonStringEnumConverter() },
        DefaultIgnoreCondition = JsonIgnoreCondition.Never,
    };

    public string Folder { get; } = folder;

    public string Save(BenchResult r)
    {
        Directory.CreateDirectory(Folder);
        var stamp = r.Started.ToString("yyyyMMdd-HHmmss", CultureInfo.InvariantCulture);
        var path = System.IO.Path.Combine(Folder, stamp + ".json");
        for (var n = 2; File.Exists(path); n++) path = System.IO.Path.Combine(Folder, $"{stamp}-{n}.json");
        var tmp = path + ".tmp";
        File.WriteAllText(tmp, JsonSerializer.Serialize(r, Json), new UTF8Encoding(false));
        File.Move(tmp, path);
        r.Path = path;
        return path;
    }

    public static BenchResult Load(string path)
    {
        try
        {
            var r = JsonSerializer.Deserialize<BenchResult>(File.ReadAllText(path), Json) ?? throw new InvalidDataException($"{path} is empty.");
            r.Path = path;
            return r;
        }
        catch (JsonException e) { throw new InvalidDataException($"{System.IO.Path.GetFileName(path)} is not a benchmark: {e.Message}"); }
    }

    /// <summary>Saved benchmarks, newest first; files that cannot be read are left out.</summary>
    public List<BenchEntry> List()
    {
        var list = new List<BenchEntry>();
        if (!Directory.Exists(Folder)) return list;
        foreach (var f in Directory.EnumerateFiles(Folder, "*.json"))
        {
            try
            {
                var r = Load(f);
                var ok = r.Runs.Count(x => x.Status == RunStatus.Ok);
                list.Add(new(f, r.Started, string.Join(" vs ", r.Candidates.Select(c => c.Name)),
                    $"{r.Started:yyyy-MM-dd HH:mm} · {ok} of {r.Runs.Count} runs good · {r.Options.WarmupSeconds:0}+{r.Options.MeasureSeconds:0} s"
                    + (r.Cancelled ? " · cancelled" : "")));
            }
            catch (Exception e) when (e is IOException or InvalidDataException or UnauthorizedAccessException) { }
        }
        return list.OrderByDescending(e => e.Started).ToList();
    }

    /// <summary>
    /// CSV: one row per run, then per candidate its mean, spread and difference from the first candidate. Not
    /// measured: an empty cell. Invariant culture (a dot for decimals), so it reads the same everywhere.
    /// </summary>
    public static string ToCsv(BenchResult r)
    {
        var sb = new StringBuilder();
        var head = new List<string> { "row", "candidate", "repeat", "status", "failure", "frames", "first_picture_s" };
        head.AddRange(BenchStats.Metrics.Select(m => m.Id));
        Line(sb, head);
        foreach (var run in r.Runs)
        {
            var cells = new List<string>
            {
                "run", r.Candidates[run.Candidate].Name, (run.Repeat + 1).ToString(CultureInfo.InvariantCulture), run.Status.ToString(),
                run.Failure ?? "", run.Metrics.Frames.ToString(CultureInfo.InvariantCulture), Num(run.FirstFrameSeconds),
            };
            cells.AddRange(BenchStats.Metrics.Select(m => Num(m.Get(run.Metrics))));
            Line(sb, cells);
        }
        for (var c = 0; c < r.Candidates.Count; c++)
        {
            var stats = BenchStats.Metrics.Select(m => BenchStats.Of(r, c, m)).ToList();
            foreach (var (kind, pick) in new (string, Func<int, MetricStats, string>)[]
                     {
                         ("mean", (_, s) => Num(s.Mean)),
                         ("spread", (_, s) => Num(s.Spread)),
                         ("diff_vs_first", (i, s) => c == 0 ? "" : Num(BenchStats.Compare(BenchStats.Metrics[i], BenchStats.Of(r, 0, BenchStats.Metrics[i]), s).Delta)),
                         ("within_noise", (i, s) => c == 0 ? "" : BenchStats.Compare(BenchStats.Metrics[i], BenchStats.Of(r, 0, BenchStats.Metrics[i]), s).WithinNoise switch { true => "yes", false => "no", null => "" }),
                     })
            {
                var cells = new List<string> { kind, r.Candidates[c].Name, "", "", "", "", "" };
                cells.AddRange(stats.Select((s, i) => pick(i, s)));
                Line(sb, cells);
            }
        }
        return sb.ToString();
    }

    public static void ExportCsv(BenchResult r, string path) => File.WriteAllText(path, ToCsv(r), new UTF8Encoding(true));

    private static string Num(double? v) => v is double d ? d.ToString("0.###", CultureInfo.InvariantCulture) : "";

    private static void Line(StringBuilder sb, IEnumerable<string> cells) =>
        sb.AppendJoin(',', cells.Select(c => c.IndexOfAny([',', '"', '\n', '\r']) >= 0 ? "\"" + c.Replace("\"", "\"\"", StringComparison.Ordinal) + "\"" : c)).Append("\r\n");
}
