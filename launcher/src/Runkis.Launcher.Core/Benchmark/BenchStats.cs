using System.Globalization;

namespace Runkis.Launcher.Core;

/// <summary>
/// A figure a benchmark compares. <see cref="Floor"/> is the smallest difference that can count: 0.5 fps for the frame
/// rates (README: four identical runs vary by about ±0.4 fps); the other floors are the launcher's own, in the
/// metric's unit, so a run with no spread does not make every 0.1 ms "real".
/// </summary>
public sealed record BenchMetric(string Id, string Name, string Unit, bool HigherIsBetter, double Floor, string Format, Func<RunMetrics, double?> Get);

/// <summary>Mean and spread of one metric over a candidate's good runs. Spread: half the range (max − min) / 2.</summary>
public sealed record MetricStats(double? Mean, double? Spread, int N);

/// <summary>A candidate's metric against the first candidate's.</summary>
public sealed record MetricDiff(double? Delta, double? Noise, bool? WithinNoise);

public static class BenchStats
{
    public static readonly IReadOnlyList<BenchMetric> Metrics =
    [
        new("avg_fps", "Average fps", "fps", true, 0.5, "0.0", m => m.AvgFps),
        new("low1_fps", "1 % low", "fps", true, 0.5, "0.0", m => m.Low1Fps),
        new("median_ms", "Median frame", "ms", false, 1.0, "0.0", m => m.MedianMs),
        new("hitches", "Hitches ≥100 ms", "", false, 1.0, "0.#", m => m.Hitches),
        new("worst_ms", "Worst frame", "ms", false, 10.0, "0", m => m.WorstMs),
        new("preparing_ms", "Preparing", "ms", false, 0.5, "0.0", m => m.PreparingMs),
        new("gpu_ms", "On the GPU", "ms", false, 0.5, "0.0", m => m.GpuMs),
        new("copy_back_ms", "Copying back", "ms", false, 0.5, "0.0", m => m.CopyBackMs),
        new("peak_private_mb", "Peak private", "MB", false, 64, "0", m => m.PeakPrivateMb),
        new("peak_vram_mb", "Peak VRAM", "MB", false, 64, "0", m => m.PeakVramMb),
    ];

    public static MetricStats Of(IEnumerable<double?> values)
    {
        var v = values.OfType<double>().ToList();
        if (v.Count == 0) return new(null, null, 0);
        return new(v.Average(), v.Count >= 2 ? (v.Max() - v.Min()) / 2 : null, v.Count);
    }

    public static MetricStats Of(BenchResult r, int candidate, BenchMetric m) =>
        Of(r.Runs.Where(x => x.Candidate == candidate && x.Status == RunStatus.Ok).Select(x => m.Get(x.Metrics)));

    /// <summary>The noise rule: a difference counts only from max(floor, 2 × the larger spread) on.</summary>
    public static double NoiseLimit(double floor, double spreadA, double spreadB) => Math.Max(floor, 2 * Math.Max(spreadA, spreadB));

    public static bool IsWithinNoise(double delta, double floor, double spreadA, double spreadB) => Math.Abs(delta) < NoiseLimit(floor, spreadA, spreadB);

    /// <summary>Null parts where a mean is missing; no noise call with fewer than two runs on either side.</summary>
    public static MetricDiff Compare(BenchMetric m, MetricStats baseline, MetricStats other)
    {
        if (baseline.Mean is not double a || other.Mean is not double b) return new(null, null, null);
        var delta = b - a;
        if (baseline.Spread is not double sa || other.Spread is not double sb) return new(delta, null, null);
        return new(delta, NoiseLimit(m.Floor, sa, sb), IsWithinNoise(delta, m.Floor, sa, sb));
    }

    public static string Num(double? v, string format) => v is double d ? d.ToString(format, CultureInfo.InvariantCulture) : "—";
}

public sealed record BenchCell(string Value, string Diff, bool? WithinNoise, bool? Better);

public sealed record BenchRow(string Candidate, string Runs, IReadOnlyList<BenchCell> Cells);

/// <summary>The table and verdicts of a result, as text; "—" where nothing was measured.</summary>
public sealed record BenchReport(IReadOnlyList<string> Columns, IReadOnlyList<BenchRow> Rows, IReadOnlyList<string> Verdicts)
{
    public static BenchReport Of(BenchResult r)
    {
        var columns = BenchStats.Metrics.Select(m => m.Unit.Length > 0 ? $"{m.Name} ({m.Unit})" : m.Name).ToList();
        var stats = r.Candidates.Select((_, c) => BenchStats.Metrics.Select(m => BenchStats.Of(r, c, m)).ToList()).ToList();
        var rows = new List<BenchRow>();
        for (var c = 0; c < r.Candidates.Count; c++)
        {
            var cells = new List<BenchCell>();
            for (var i = 0; i < BenchStats.Metrics.Count; i++)
            {
                var m = BenchStats.Metrics[i];
                var s = stats[c][i];
                var value = s.Mean is double mean
                    ? BenchStats.Num(mean, m.Format) + (s.Spread is double sp ? " ± " + BenchStats.Num(sp, m.Format) : "")
                    : "—";
                if (c == 0) { cells.Add(new(value, "baseline", null, null)); continue; }
                var d = BenchStats.Compare(m, stats[0][i], s);
                var better = d.Delta is double dd ? (m.HigherIsBetter ? dd > 0 : dd < 0) : (bool?)null;
                var diff = d.Delta is double delta
                    ? (delta >= 0 ? "+" : "") + BenchStats.Num(delta, m.Format)
                      + (d.WithinNoise switch { true => " · within noise", false => better == true ? " · better" : delta == 0 ? "" : " · worse", null => " · noise unknown" })
                    : "—";
                cells.Add(new(value, diff, d.WithinNoise, d.WithinNoise == false ? better : null));
            }
            var runs = r.Runs.Where(x => x.Candidate == c).ToList();
            var ok = runs.Count(x => x.Status == RunStatus.Ok);
            var failed = runs.Count(x => x.Status == RunStatus.Failed);
            rows.Add(new(r.Candidates[c].Name, $"{ok} of {runs.Count} good" + (failed > 0 ? $", {failed} failed" : ""), cells));
        }
        return new(columns, rows, VerdictLines(r, stats));
    }

    private static List<string> VerdictLines(BenchResult r, List<List<MetricStats>> stats)
    {
        var lines = new List<string>();
        if (r.Candidates.Count == 0) return lines;
        var a = r.Candidates[0].Name;
        for (var i = 0; i < BenchStats.Metrics.Count; i++)
        {
            var m = BenchStats.Metrics[i];
            var unit = m.Unit.Length > 0 ? " " + m.Unit : "";
            if (stats[0][i].Mean is not double baseMean) { lines.Add($"{m.Name}: — (not measured for {a})."); continue; }
            if (r.Candidates.Count == 1)
            {
                var s = stats[0][i];
                lines.Add($"{m.Name}: {a} {BenchStats.Num(baseMean, m.Format)}{unit}" + (s.Spread is double sp ? $" ± {BenchStats.Num(sp, m.Format)} over {s.N} runs." : $" (one run: no spread)."));
                continue;
            }
            var parts = new List<string>();
            var within = new List<string>();
            for (var c = 1; c < r.Candidates.Count; c++)
            {
                var name = r.Candidates[c].Name;
                var d = BenchStats.Compare(m, stats[0][i], stats[c][i]);
                if (d.Delta is not double delta) { parts.Add($"{name} —"); continue; }
                if (d.WithinNoise == true) { within.Add(name); continue; }
                var better = m.HigherIsBetter ? delta > 0 : delta < 0;
                var pct = baseMean != 0 ? $" ({(delta / baseMean * 100).ToString("+0;-0", CultureInfo.InvariantCulture)} %)" : "";
                var size = $"{BenchStats.Num(Math.Abs(delta), m.Format)}{unit}{pct}";
                parts.Add(d.WithinNoise == null
                    ? $"{name} {(delta >= 0 ? "+" : "−")}{size}, noise unknown (needs 2+ good runs each)"
                    : $"{name} {(better ? "better" : "worse")} by {size}, beyond noise (±{BenchStats.Num(d.Noise, m.Format)})");
            }
            if (within.Count > 0) parts.Add($"{string.Join(", ", within)} within noise");
            var all = parts.Count == 1 && within.Count == r.Candidates.Count - 1;
            lines.Add(all ? $"{m.Name}: no difference from {a} beyond noise." : $"{m.Name} vs {a}: {string.Join("; ", parts)}.");
        }
        return lines;
    }
}
