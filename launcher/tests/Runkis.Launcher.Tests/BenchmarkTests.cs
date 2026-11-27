using System.Globalization;
using System.Text;
using System.Text.RegularExpressions;
using Runkis.Launcher.Core;
using Xunit;

namespace Runkis.Launcher.Tests;

internal static class Repo
{
    public static string Root()
    {
        var d = new DirectoryInfo(AppContext.BaseDirectory);
        while (d != null && !File.Exists(Path.Combine(d.FullName, RepoLayout.RootMarker))) d = d.Parent;
        Assert.True(d != null, "the tests run inside the source tree");
        return d!.FullName;
    }

    public static string Read(params string[] parts) => File.ReadAllText(Path.Combine([Root(), .. parts]));

    /// <summary>A Rust string literal from its first characters, with escapes and line continuations resolved.</summary>
    public static string RustLiteral(string src, string startsWith)
    {
        var i = src.IndexOf("\"" + startsWith, StringComparison.Ordinal);
        Assert.True(i >= 0, $"host.rs no longer has a literal starting {startsWith}");
        var sb = new StringBuilder();
        for (var j = i + 1; ; j++)
        {
            var ch = src[j];
            if (ch == '"') break;
            if (ch == '\\')
            {
                var next = src[j + 1];
                if (next is '\r' or '\n')
                {
                    j++;
                    while (char.IsWhiteSpace(src[j + 1])) j++;
                    continue;
                }
                sb.Append(next switch { 'n' => '\n', 't' => '\t', _ => next });
                j++;
                continue;
            }
            sb.Append(ch);
        }
        return sb.ToString();
    }
}

public class LayerLineTests
{
    private const string Prefix = "2026-11-27T08:00:05.123456Z  INFO victory_gpu::host: ";

    /// <summary>The format string in host.rs, filled the way Rust's format! fills it, parses back to the same figures.</summary>
    [Fact]
    public void The_fps_line_parses_as_host_rs_writes_it()
    {
        var format = Repo.RustLiteral(Repo.Read("crates", "victory-gpu", "src", "host.rs"), "{fps:.1} fps, {ms:.1} ms/frame (");
        var values = new Dictionary<string, double>
        {
            ["fps"] = 18.3, ["ms"] = 54.6, ["cpu"] = 21.4, ["copy_ms"] = 3.2, ["copy_mb"] = 40.1, ["texture_ms"] = 5.5,
            ["texture_mb"] = 12.0, ["gpu"] = 30.2, ["back_ms"] = 9.9, ["back_mb"] = 88.8,
        };
        var line = Regex.Replace(format, @"\{([a-z_0-9]+)(:[^}]*)?\}", m =>
        {
            var v = values.GetValueOrDefault(m.Groups[1].Value, 7);
            var spec = m.Groups[2].Value;
            return spec == ":.1" ? v.ToString("0.0", CultureInfo.InvariantCulture)
                : spec == ":.0" ? v.ToString("0", CultureInfo.InvariantCulture)
                : spec == ":.2" ? v.ToString("0.00", CultureInfo.InvariantCulture) : v.ToString(CultureInfo.InvariantCulture);
        });
        Assert.DoesNotContain("{", line);
        var f = LayerLines.ParseFps(Prefix + line);
        Assert.NotNull(f);
        Assert.Equal(new FpsLine(18.3, 54.6, 21.4, 3.2, 40.1, 5.5, 12.0, 30.2, 9.9, 88.8), f);
    }

    [Fact]
    public void A_logged_fps_line_parses_and_others_do_not()
    {
        var f = LayerLines.ParseFps(Prefix + "17.9 fps, 55.8 ms/frame (24.0 preparing: copies 4.1 ms 61.3 MB, textures 1.0 ms 2.5 MB; 28.7 on the GPU; 21.9 copying back 310.0 MB), 2817 draws/frame, dropped 0 without a host pipeline (0 being made) and 12 indirect; heap buffers 0.0 passes 0.0 ms;");
        Assert.Equal(new FpsLine(17.9, 55.8, 24.0, 4.1, 61.3, 1.0, 2.5, 28.7, 21.9, 310.0), f);
        Assert.Null(LayerLines.ParseFps("Runkis | VIctory 18 fps 55.0 ms/frame 900 draws/frame"));
        Assert.Null(LayerLines.ParseFps(Prefix + "dropped 1.0/frame without a host pipeline: x"));
        Assert.Null(LayerLines.ParseFps(Prefix + "NaN fps, inf ms/frame (0.0 preparing: copies"));
    }

    /// <summary>host.rs memory_text: the prefix, then " {name}={} {name}_peak={}" for each field of memory_line.</summary>
    [Fact]
    public void The_memory_line_parses_as_host_rs_writes_it()
    {
        var src = Repo.Read("crates", "victory-gpu", "src", "host.rs");
        Assert.Contains("text.push_str(\"memory by subsystem:\");", src);
        Assert.Contains("write!(text, \" {name}={} {name}_peak={}\", bytes >> 20, peak >> 20);", src);
        var body = src[src.IndexOf("fn memory_line(", StringComparison.Ordinal)..];
        body = body[..body.IndexOf("tracing::info!", StringComparison.Ordinal)];
        var names = Regex.Matches(body, @"\(""([a-z_]+)"",").Select(m => m.Groups[1].Value).ToList();
        Assert.Contains(LayerLines.PrivateField, names);
        Assert.Contains(LayerLines.VramField, names);
        var line = new StringBuilder("2026-11-27T08:00:05Z  INFO victory_gpu::host: " + LayerLines.MemoryPrefix);
        for (var i = 0; i < names.Count; i++) line.Append(CultureInfo.InvariantCulture, $" {names[i]}={100 + i} {names[i]}_peak={1000 + i}");
        var fields = LayerLines.ParseMemory(line.ToString());
        Assert.NotNull(fields);
        for (var i = 0; i < names.Count; i++)
        {
            Assert.Equal(100 + i, fields![names[i]]);
            Assert.Equal(1000 + i, fields[names[i] + "_peak"]);
        }
    }

    [Fact]
    public void The_memory_line_of_host_rs_own_test_parses()
    {
        var f = LayerLines.ParseMemory("memory by subsystem: windows=300 windows_peak=300 gpu_used=5 gpu_used_peak=5");
        Assert.Equal("gpu_used=5 gpu_used_peak=5 windows=300 windows_peak=300", string.Join(" ", f!.OrderBy(kv => kv.Key, StringComparer.Ordinal).Select(kv => $"{kv.Key}={kv.Value}")));
        Assert.Null(LayerLines.ParseMemory("2026 INFO placed texture views: 3 writes"));
    }
}

public class KeyScriptTests
{
    /// <summary>The parser mirrors gameinput.rs scripted(); if that changes, this fails first.</summary>
    [Fact]
    public void The_layer_still_reads_the_keys_this_way()
    {
        var src = Repo.Read("crates", "victory-hle", "src", "modules", "gameinput.rs");
        foreach (var arm in new[] { "\"Enter\" => 0x0D", "\"Esc\" => 0x1B", "\"Tab\" => 0x09", "\"Back\" => 0x08", "\"Up\" => 0x26", "\"Down\" => 0x28", "\"Left\" => 0x25", "\"Right\" => 0x27" })
            Assert.Contains(arm, src);
        Assert.Contains("std::env::var(\"VICTORY_KEYS\")", src);
        Assert.Contains("list.split(',')", src);
        Assert.Contains("entry.split_once(':')?", src);
        Assert.Contains("at.split_once('-')", src);
        Assert.Contains("time(at)? + Duration::from_millis(200)", src);
        Assert.Contains("(from..to).contains(&now)", src);
    }

    [Fact]
    public void Entries_parse_as_the_layer_reads_them()
    {
        var p = KeyScript.Parse("30:Enter,35:x, 40-45.5:W ,+5:1,.5:Esc");
        Assert.Empty(p.Skipped);
        Assert.Equal(
        new[]
        {
            new KeyPress(30, 30.2, "Enter", 0x0D), new KeyPress(35, 35.2, "X", (byte)'X'), new KeyPress(40, 45.5, "W", (byte)'W'),
            new KeyPress(5, 5.2, "1", (byte)'1'), new KeyPress(0.5, 0.7, "Esc", 0x1B),
        }, p.Keys.Select(k => k with { To = Math.Round(k.To, 6) }));
    }

    [Theory]
    [InlineData("abc")]           // no ':'
    [InlineData("10:enter")]      // names are case-sensitive
    [InlineData("10:F1")]
    [InlineData("x:A")]
    [InlineData("1e-3:A")]        // the '-' splits it into "1e" and "3"
    [InlineData("10-:A")]
    [InlineData("inf:A")]
    public void Entries_the_layer_skips_are_errors(string entry)
    {
        Assert.Single(KeyScript.Parse(entry).Skipped);
        Assert.False(KeyScript.Check(entry, 40, 90).Ok);
    }

    [Fact]
    public void The_editor_text_becomes_one_comma_list()
    {
        Assert.Equal("30:Enter,35:X,36:Y", KeyScript.ToVariable("30:Enter   # past the title screen\r\n35:X, 36:Y\n\n,\n"));
        Assert.Equal("", KeyScript.ToVariable("  # nothing\n"));
        Assert.Empty(KeyScript.Parse("").Keys);
        Assert.Empty(KeyScript.Parse("").Skipped);
    }

    [Fact]
    public void The_script_is_checked_against_the_warm_up()
    {
        var c = KeyScript.Check("5:Enter,8-3:W,140:X", 40, 90);
        Assert.True(c.Ok);
        Assert.Equal(2, c.Warnings.Count);
        Assert.Contains(c.Warnings, w => w.Contains("never held"));
        Assert.Contains(c.Warnings, w => w.Contains("after the run is stopped"));
        Assert.Contains(c.Notes, n => n.Contains("measured window sees no scripted input"));
        Assert.Contains(KeyScript.Check("30:Enter,45-100:W", 40, 90).Notes, n => n.Contains("1 is held after 40 s"));
    }
}

public class BenchStatsTests
{
    [Fact]
    public void Only_frames_inside_the_measured_window_count()
    {
        var frames = new[] { 39.9, 40.0, 40.1, 129.99, 130.0, 130.1 }.Select(t => new FrameSample(t, 50, 1));
        Assert.Equal(new[] { 40.1, 129.99, 130.0 }, FramePacing.Window(frames, 40, 130).Select(f => f.At));
    }

    [Fact]
    public void Pacing_figures_match_their_definitions()
    {
        var ms = Enumerable.Repeat(50.0, 200).Concat([150.0, 150.0, 0.0]).ToList();
        var f = FramePacing.Of(ms);
        Assert.Equal(202, f.Frames);                       // the 0 gap is left out
        Assert.Equal(1000.0 * 202 / 10300, f.AvgFps!.Value, 6);
        Assert.Equal(50.0, f.MedianMs);
        Assert.Equal(1000.0 / 150, f.Low1Fps!.Value, 6);   // the slowest 1 %: 2 of 202 frames
        Assert.Equal(150.0, f.WorstMs);
        Assert.Equal(2, f.Hitches);
        Assert.Equal(45.0, FramePacing.Of([40, 50]).MedianMs);
        var none = FramePacing.Of([50]);
        Assert.Null(none.AvgFps);
        Assert.Null(none.Hitches);

        var live = new FrameStats();
        for (var i = 0; i < ms.Count; i++) live.Add(new FrameSample(i * 0.01, ms[i], 0));
        var s = live.Summary(100);
        Assert.Equal(f.AvgFps, s.Fps);
        Assert.Equal(f.Low1Fps, s.Low1Fps);
    }

    [Fact]
    public void Mean_and_spread_over_repeats()
    {
        Assert.Equal(new MetricStats(18.5, 0.5, 2), BenchStats.Of([18.0, 19.0, null]));
        Assert.Equal(new MetricStats(18.0, null, 1), BenchStats.Of([18.0]));
        Assert.Equal(new MetricStats(null, null, 0), BenchStats.Of([null]));
    }

    [Theory]
    [InlineData(0.4, 0.1, 0.1, true)]     // under the 0.5 fps floor
    [InlineData(0.6, 0.1, 0.1, false)]
    [InlineData(-0.6, 0.1, 0.1, false)]
    [InlineData(0.9, 0.5, 0.2, true)]     // under 2 x the larger spread (1.0)
    [InlineData(1.1, 0.2, 0.5, false)]
    [InlineData(0.5, 0, 0, false)]        // "under" the limit: equal is not within
    public void The_noise_rule(double delta, double spreadA, double spreadB, bool within) =>
        Assert.Equal(within, BenchStats.IsWithinNoise(delta, 0.5, spreadA, spreadB));

    [Fact]
    public void No_noise_call_without_two_runs_each()
    {
        var fps = BenchStats.Metrics[0];
        var d = BenchStats.Compare(fps, new(18, null, 1), new(20, 0.1, 2));
        Assert.Equal(2.0, d.Delta);
        Assert.Null(d.WithinNoise);
        Assert.Equal(new MetricDiff(null, null, null), BenchStats.Compare(fps, new(null, null, 0), new(20, 0.1, 2)));
    }

    private static BenchRun Ok(int c, int r, double fps) => new() { Candidate = c, Repeat = r, Status = RunStatus.Ok, Metrics = new RunMetrics { AvgFps = fps, Frames = 1000 } };

    [Fact]
    public void The_report_compares_with_the_first_candidate_and_shows_dashes_for_missing_figures()
    {
        var r = new BenchResult
        {
            Candidates = [new() { Name = "A" }, new() { Name = "B" }, new() { Name = "C" }],
            Runs = [Ok(0, 0, 18.0), Ok(1, 0, 19.5), Ok(2, 0, 18.1), Ok(0, 1, 18.2), Ok(1, 1, 19.7), Ok(2, 1, 18.3),
                    new() { Candidate = 2, Repeat = 2, Status = RunStatus.Failed, Failure = "crash" }],
        };
        var report = BenchReport.Of(r);
        Assert.Equal("18.1 ± 0.1", report.Rows[0].Cells[0].Value);
        Assert.Equal("+1.5 · better", report.Rows[1].Cells[0].Diff);
        Assert.False(report.Rows[1].Cells[0].WithinNoise);
        Assert.True(report.Rows[2].Cells[0].WithinNoise);
        Assert.Equal("2 of 3 good, 1 failed", report.Rows[2].Runs);
        Assert.Equal("—", report.Rows[1].Cells[1].Value);  // 1 % low was never measured
        Assert.Equal("—", report.Rows[1].Cells[1].Diff);
        Assert.Contains("B better by 1.5 fps (+8 %), beyond noise (±0.5)", report.Verdicts[0]);
        Assert.Contains("C within noise", report.Verdicts[0]);
        Assert.Equal("1 % low: — (not measured for A).", report.Verdicts[1]);
    }

    [Fact]
    public void Runs_are_interleaved()
    {
        Assert.Equal(new[] { (0, 0), (1, 0), (0, 1), (1, 1) }, BenchOrder.Interleave(2, 2));
        Assert.Equal(new[] { (0, 0), (1, 0), (2, 0), (0, 1), (1, 1), (2, 1), (0, 2), (1, 2), (2, 2) }, BenchOrder.Interleave(3, 3));
    }

    [Fact]
    public void Raw_overrides_must_be_layer_switches()
    {
        var extra = BenchCandidate.ParseExtra("VICTORY_A=1\r\nvictory_b=2\nVICTORY_FRAME_LOG=x\nFOO=1\n\n  VICTORY_C=  \nVICTORY_A=3\nVICTORY_KEYS=1:A", out var errors);
        Assert.Equal(new Dictionary<string, string> { ["VICTORY_A"] = "1", ["VICTORY_C"] = "" }, extra);
        Assert.Equal(5, errors.Count);
    }

    [Fact]
    public void A_plan_takes_the_extra_settings_and_its_own_logs()
    {
        var plan = new PlanBuilder(new RepoLayout(@"C:\r")).Compose(new LaunchProfile { GameExe = @"C:\g\game.exe" }, RunMode.Play,
            new ScriptPreset { Victory = @"C:\r\target\diag\victory.exe", Args = ["run"], Settings = new() { ["VICTORY_X"] = "1" } }, DateTime.Now);
        var b = plan.With(new Dictionary<string, string> { ["VICTORY_X"] = "2", ["VICTORY_KEYS"] = "1:A" }, @"C:\l\t-bench.log", @"C:\l\t-bench.frames.txt");
        Assert.Equal("2", b.Settings["VICTORY_X"]);
        Assert.Equal("1:A", b.Settings["VICTORY_KEYS"]);
        Assert.Equal(@"C:\l\t-bench.frames.txt", b.Settings["VICTORY_FRAME_LOG"]);
        Assert.Contains("VICTORY_X", b.Overridden);
        Assert.Equal(@"C:\l\t-bench.log", b.LogPath);
        Assert.Equal("1", plan.Settings["VICTORY_X"]);
    }

    [Fact]
    public void Results_are_saved_listed_and_exported()
    {
        using var d = new TempDir();
        var store = new BenchmarkStore(d.Path);
        var r = new BenchResult
        {
            Started = new DateTime(2026, 11, 27, 10, 0, 0), Candidates = [new() { Name = "A, quoted \"x\"" }, new() { Name = "B" }],
            Runs = [Ok(0, 0, 18), Ok(1, 0, 19), new() { Candidate = 0, Repeat = 1, Status = RunStatus.Failed, Failure = "No picture" }],
        };
        var path = store.Save(r);
        Assert.Equal("20261127-100000.json", Path.GetFileName(path));
        Assert.NotEqual(path, store.Save(r));
        var back = BenchmarkStore.Load(path);
        Assert.Equal(3, back.Runs.Count);
        Assert.Null(back.Runs[2].Metrics.AvgFps);
        Assert.Equal(2, store.List().Count);
        var csv = BenchmarkStore.ToCsv(back).Split("\r\n");
        Assert.StartsWith("row,candidate,repeat,status,failure,frames,first_picture_s,avg_fps,low1_fps", csv[0]);
        Assert.StartsWith("run,\"A, quoted \"\"x\"\"\",1,Ok,,1000,,18,,", csv[1]);
        Assert.DoesNotContain(csv, l => l.Contains('—'));
        Assert.Contains(csv, l => l.StartsWith("diff_vs_first,B,,,,,,1,", StringComparison.Ordinal));
    }
}

/// <summary>A victory process on a fake clock: frames from its first picture on, the layer's five-second lines, a crash.</summary>
internal sealed class FakeSession(FakeWorld world, string name) : IBenchSession
{
    public double? FirstPictureAfter { get; init; } = 3;
    public double? CrashAfter { get; init; }
    public double FrameMs { get; init; } = 50;
    /// <summary>Frame-log seconds at which one frame takes 150 ms.</summary>
    public double[] HitchesAt { get; init; } = [];
    public string Name { get; } = name;
    public bool Stopped { get; private set; }
    public bool Disposed { get; private set; }
    private DateTime started;
    private double next, nextLine = 5;
    private bool begun, ended;

    public event Action<LogLine>? Line;
    public SessionState State => !begun ? SessionState.Starting : ended || Crashed ? SessionState.Exited : SessionState.Running;
    private bool Crashed => CrashAfter is double c && (world.Now - started).TotalSeconds >= c;
    public string? ExitText => Crashed ? "Access violation (0xC0000005)." : ended ? "Stopped by the launcher." : null;
    public IReadOnlyCollection<Problem> Problems => Crashed ? [new Problem("av", Severity.Error, "The game crashed", "an access violation", "", "0xC0000005")] : [];
    public string? LogPath => $"{Name}.log";
    public string? FrameLogPath => $"{Name}.frames.txt";

    public void Start()
    {
        Assert.All(world.Sessions.Where(s => s != this), s => Assert.True(s.ended || s.Crashed, "two victory processes at once"));
        started = world.Now;
        begun = true;
    }

    public IReadOnlyList<FrameSample> ReadFrames()
    {
        var list = new List<FrameSample>();
        if (FirstPictureAfter is not double first || ended) return list;
        var upTo = (Math.Min((world.Now - started).TotalSeconds, CrashAfter ?? double.MaxValue)) - first;
        while (next <= upTo)
        {
            var ms = list.Count == 0 && next == 0 ? 0 : HitchesAt.Any(h => Math.Abs(h - next) < FrameMs / 2000) ? 150 : FrameMs;
            list.Add(new FrameSample(next, ms, 900));
            while (nextLine <= next)
            {
                var at = started.AddSeconds(first + nextLine);
                // Private memory is high in the warm-up only, so a peak from there would show.
                var priv = nextLine < 10 ? 50000 : 9000 + (long)nextLine;
                Line?.Invoke(new LogLine(at, true, $"{at:O}  INFO victory_gpu::host: memory by subsystem: private={priv} private_peak={priv} gpu_used=6000 gpu_used_peak=6000"));
                var cpu = nextLine < 10 ? 99.0 : 12.0;
                Line?.Invoke(new LogLine(at, true, FormattableString.Invariant($"{at:O}  INFO victory_gpu::host: 20.0 fps, 50.0 ms/frame ({cpu:0.0} preparing: copies 3.0 ms 40.0 MB, textures 2.0 ms 10.0 MB; 30.0 on the GPU; 8.0 copying back 50.0 MB), 900 draws/frame, dropped 0")));
                nextLine += 5;
            }
            next += (list[^1].Ms > 0 ? list[^1].Ms : FrameMs) / 1000;
        }
        return list;
    }

    public Task StopAsync(TimeSpan grace) { Stopped = true; ended = true; return Task.CompletedTask; }
    public void Dispose() => Disposed = true;
}

internal sealed class FakeWorld
{
    public DateTime Now = new(2026, 11, 27, 8, 0, 0, DateTimeKind.Utc);
    public List<FakeSession> Sessions { get; } = [];
    public Func<string, int, FakeSession> Make { get; set; }
    public DateTime? CancelAt { get; set; }
    public CancellationTokenSource Cts { get; } = new();

    public FakeWorld() => Make = (n, _) => new FakeSession(this, n);

    public BenchmarkRunner Runner() => new((c, keys, ct) =>
    {
        Keys.Add(keys);
        var s = Make(c.Name, Sessions.Count(x => x.Name == c.Name));
        Sessions.Add(s);
        return Task.FromResult<IBenchSession>(s);
    }, () => Now, (span, ct) =>
    {
        Now += span;
        if (CancelAt is DateTime c && Now >= c) Cts.Cancel();
        ct.ThrowIfCancellationRequested();
        return Task.CompletedTask;
    });

    public List<string> Keys { get; } = [];
}

public class BenchmarkRunnerTests
{
    private static readonly BenchOptions Short = new() { WarmupSeconds = 10, MeasureSeconds = 20, Repeats = 2, Keys = "1:Enter" };
    private static BenchCandidate C(string name) => new() { Name = name };

    [Fact]
    public async Task Runs_go_interleaved_and_measure_the_window_only()
    {
        var w = new FakeWorld();
        w.Make = (n, _) => new FakeSession(w, n) { HitchesAt = [5.0, 15.0] };   // one in the warm-up, one measured
        var result = await w.Runner().RunAsync([C("A"), C("B")], Short);

        Assert.Equal(new[] { "A", "B", "A", "B" }, w.Sessions.Select(s => s.Name));
        Assert.All(w.Sessions, s => Assert.True(s.Stopped && s.Disposed));
        Assert.All(w.Keys, k => Assert.Equal("1:Enter", k));
        Assert.False(result.Cancelled);
        Assert.All(result.Runs, r => Assert.Equal(RunStatus.Ok, r.Status));
        Assert.Equal(new[] { 0, 1, 0, 1 }, result.Runs.Select(r => r.Candidate));
        Assert.Equal(new[] { 0, 0, 1, 1 }, result.Runs.Select(r => r.Repeat));
        var m = result.Runs[0].Metrics;
        Assert.Equal(1, m.Hitches);
        Assert.Equal(150.0, m.WorstMs);
        Assert.Equal(50.0, m.MedianMs);
        Assert.InRange(m.Frames, 395, 400);                       // 20 s at 20 fps, one frame of them 150 ms
        Assert.InRange(m.AvgFps!.Value, 19.8, 20.0);
        Assert.Equal(12.0, m.PreparingMs);                        // not the 99 of the warm-up's lines
        Assert.Equal(30.0, m.GpuMs);
        Assert.Equal(8.0, m.CopyBackMs);
        Assert.InRange(m.PeakPrivateMb!.Value, 9015, 9030);       // the warm-up's 50000 is left out
        Assert.Equal(6000L, m.PeakVramMb);
        Assert.InRange(result.Runs[0].FirstFrameSeconds!.Value, 2.9, 3.1);
        Assert.Equal("baseline", BenchReport.Of(result).Rows[0].Cells[0].Diff);
    }

    [Fact]
    public async Task A_crash_fails_that_run_and_the_next_goes_on()
    {
        var w = new FakeWorld();
        w.Make = (n, i) => new FakeSession(w, n) { CrashAfter = n == "A" && i == 0 ? 20 : null };
        var result = await w.Runner().RunAsync([C("A"), C("B")], Short);

        Assert.Equal(4, result.Runs.Count);
        var crashed = result.Runs[0];
        Assert.Equal(RunStatus.Failed, crashed.Status);
        Assert.Contains("Access violation (0xC0000005).", crashed.Failure);
        Assert.Contains("before the measurement was done", crashed.Failure);
        Assert.Contains(crashed.Problems, p => p.Contains("The game crashed"));
        Assert.Null(crashed.Metrics.AvgFps);
        Assert.All(result.Runs.Skip(1), r => Assert.Equal(RunStatus.Ok, r.Status));
        Assert.Equal("1 of 2 good, 1 failed", BenchReport.Of(result).Rows[0].Runs);
    }

    [Fact]
    public async Task A_run_without_a_picture_times_out()
    {
        var w = new FakeWorld();
        w.Make = (n, _) => new FakeSession(w, n) { FirstPictureAfter = n == "A" ? null : 3 };
        var start = w.Now;
        var result = await w.Runner().RunAsync([C("A"), C("B")], Short);

        var a = result.Runs[0];
        Assert.Equal(RunStatus.Failed, a.Status);
        Assert.Contains("No picture within 130 s", a.Failure);
        Assert.True(w.Sessions[0].Stopped);
        Assert.Null(a.FirstFrameSeconds);
        Assert.Equal(RunStatus.Ok, result.Runs[1].Status);
        // The first run gave up at warm-up + 120 s, not later.
        Assert.InRange((a.Ended - a.Started).TotalSeconds, 0, 5);
        // 130 s for each run of A that never showed a picture, about 33 s for each of B.
        Assert.InRange((w.Now - start).TotalSeconds, 2 * 130 + 2 * 30, 2 * 131 + 2 * 40);
    }

    [Fact]
    public async Task Cancel_stops_the_current_run_and_starts_no_other()
    {
        var w = new FakeWorld();
        w.CancelAt = w.Now.AddSeconds(15);
        var result = await w.Runner().RunAsync([C("A"), C("B")], Short, ct: w.Cts.Token);

        Assert.True(result.Cancelled);
        Assert.Single(result.Runs);
        Assert.Equal(RunStatus.Cancelled, result.Runs[0].Status);
        Assert.Single(w.Sessions);
        Assert.True(w.Sessions[0].Stopped);
    }

    [Fact]
    public async Task Bad_input_is_refused_before_anything_starts()
    {
        var w = new FakeWorld();
        await Assert.ThrowsAsync<ArgumentException>(() => w.Runner().RunAsync([C("A")], new BenchOptions { Keys = "30:enter" }));
        await Assert.ThrowsAsync<ArgumentException>(() => w.Runner().RunAsync([C("A"), C("a")], new BenchOptions()));
        await Assert.ThrowsAsync<ArgumentException>(() => w.Runner().RunAsync([], new BenchOptions()));
        await Assert.ThrowsAsync<ArgumentException>(() => w.Runner().RunAsync([C("A")], new BenchOptions { Repeats = 0 }));
        Assert.Empty(w.Sessions);
    }
}
