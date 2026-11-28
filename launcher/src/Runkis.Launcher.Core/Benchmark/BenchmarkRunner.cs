using System.Collections.Concurrent;

namespace Runkis.Launcher.Core;

/// <summary>What the runner needs of a run of victory.exe; <see cref="EmulatorBenchSession"/> is the real one.</summary>
public interface IBenchSession : IDisposable
{
    event Action<LogLine>? Line;
    SessionState State { get; }
    string? ExitText { get; }
    IReadOnlyCollection<Problem> Problems { get; }
    string? LogPath { get; }
    string? FrameLogPath { get; }
    void Start();
    /// <summary>The frame-log lines written since the last call.</summary>
    IReadOnlyList<FrameSample> ReadFrames();
    /// <summary>Closes the game's window, and ends the process after <paramref name="grace"/>.</summary>
    Task StopAsync(TimeSpan grace);
}

/// <summary>Makes the session of one run (not started). A <see cref="PlanException"/> fails that run only.</summary>
public delegate Task<IBenchSession> BenchSessionFactory(BenchCandidate candidate, string keys, CancellationToken ct);

public enum BenchPhase { Starting, WaitingForPicture, WarmingUp, Measuring, Stopping, Done }

public sealed record BenchProgress(int Run, int Runs, string Candidate, int Repeat, BenchPhase Phase, double Seconds, double PhaseSeconds)
{
    public string Text => Phase switch
    {
        BenchPhase.Starting => $"Run {Run} of {Runs}: {Candidate} (repeat {Repeat}) · starting",
        BenchPhase.WaitingForPicture => $"Run {Run} of {Runs}: {Candidate} (repeat {Repeat}) · waiting for the first picture, {Seconds:0} of {PhaseSeconds:0} s",
        BenchPhase.WarmingUp => $"Run {Run} of {Runs}: {Candidate} (repeat {Repeat}) · warming up, {Seconds:0} of {PhaseSeconds:0} s",
        BenchPhase.Measuring => $"Run {Run} of {Runs}: {Candidate} (repeat {Repeat}) · measuring, {Seconds:0} of {PhaseSeconds:0} s",
        BenchPhase.Stopping => $"Run {Run} of {Runs}: {Candidate} (repeat {Repeat}) · stopping",
        _ => "Done",
    };
}

/// <summary>
/// Runs a benchmark: each candidate <see cref="BenchOptions.Repeats"/> times, interleaved, one victory process at a
/// time. A run warms up for <see cref="BenchOptions.WarmupSeconds"/> from its first picture (the frame log's 0), is
/// measured for <see cref="BenchOptions.MeasureSeconds"/>, then stopped the graceful way. A run that crashes, shows no
/// picture in time, or freezes is marked failed and the next one goes on.
/// </summary>
public sealed class BenchmarkRunner(BenchSessionFactory factory, Func<DateTime>? clock = null, Func<TimeSpan, CancellationToken, Task>? wait = null)
{
    private readonly Func<DateTime> now = clock ?? (() => DateTime.UtcNow);
    private readonly Func<TimeSpan, CancellationToken, Task> delay = wait ?? Task.Delay;

    public TimeSpan Poll { get; init; } = TimeSpan.FromMilliseconds(250);
    public event Action<BenchProgress>? Progress;
    public event Action<BenchRun>? RunFinished;

    public async Task<BenchResult> RunAsync(IReadOnlyList<BenchCandidate> candidates, BenchOptions options, string machine = "", CancellationToken ct = default)
    {
        var problems = options.Validate();
        if (candidates.Count == 0) problems.Add("Add at least one candidate.");
        if (candidates.Select(c => c.Name).Distinct(StringComparer.OrdinalIgnoreCase).Count() != candidates.Count) problems.Add("Candidate names must differ.");
        var keys = KeyScript.Check(options.Keys, options.WarmupSeconds, options.MeasureSeconds);
        problems.AddRange(keys.Errors);
        if (problems.Count > 0) throw new ArgumentException(string.Join(" ", problems));

        var result = new BenchResult { Started = DateTime.Now, Options = options, Candidates = candidates.ToList(), Machine = machine };
        var order = BenchOrder.Interleave(candidates.Count, options.Repeats);
        for (var i = 0; i < order.Count; i++)
        {
            var (c, r) = order[i];
            var run = await RunOneAsync(candidates[c], options, i, order.Count, r + 1, ct).ConfigureAwait(false);
            run.Order = i;
            run.Candidate = c;
            run.Repeat = r;
            result.Runs.Add(run);
            RunFinished?.Invoke(run);
            if (run.Status == RunStatus.Cancelled) { result.Cancelled = true; break; }
        }
        result.Finished = DateTime.Now;
        Progress?.Invoke(new(order.Count, order.Count, "", 0, BenchPhase.Done, 0, 0));
        return result;
    }

    private async Task<BenchRun> RunOneAsync(BenchCandidate candidate, BenchOptions o, int index, int total, int repeat, CancellationToken ct)
    {
        var run = new BenchRun { Started = DateTime.Now };
        void Report(BenchPhase phase, double seconds, double length) =>
            Progress?.Invoke(new(index + 1, total, candidate.Name, repeat, phase, seconds, length));
        Report(BenchPhase.Starting, 0, 0);

        IBenchSession session;
        try { session = await factory(candidate, o.Keys, ct).ConfigureAwait(false); }
        catch (OperationCanceledException) { return Done(run, RunStatus.Cancelled, "Cancelled before it started."); }
        catch (Exception e) when (e is PlanException or IOException or InvalidOperationException or UnauthorizedAccessException)
        {
            return Done(run, RunStatus.Failed, $"Not started: {e.Message}");
        }

        using (session)
        {
            run.LogPath = session.LogPath;
            run.FrameLogPath = session.FrameLogPath;
            var lines = new ConcurrentQueue<LogLine>();
            session.Line += lines.Enqueue;
            var frames = new List<FrameSample>();
            DateTime? zero = null;   // the wall time of the frame log's 0 (the first picture)
            var end = o.WarmupSeconds + o.MeasureSeconds;
            var firstFrameLimit = o.WarmupSeconds + o.FirstFrameGraceSeconds;
            string? failure = null;
            var status = RunStatus.Ok;

            session.Start();
            var start = now();
            if (session.State == SessionState.Failed)
                return Done(run, RunStatus.Failed, session.ExitText ?? "victory.exe did not start.", session);

            void Take()
            {
                var got = session.ReadFrames();
                if (got.Count == 0) return;
                foreach (var f in got)
                {
                    // The frame log starts again when the title restarts itself (the new process makes it anew).
                    if (frames.Count > 0 && f.At < frames[^1].At - 0.001) { frames.Clear(); zero = null; run.Restarts++; }
                    frames.Add(f);
                }
                // The newest frame was written at most a poll before it was read: the smallest estimate is the closest.
                var estimate = now() - TimeSpan.FromSeconds(frames[^1].At);
                if (zero == null || estimate < zero) zero = estimate;
            }

            try
            {
                while (true)
                {
                    await delay(Poll, ct).ConfigureAwait(false);
                    Take();
                    if (session.State is SessionState.Exited or SessionState.Failed)
                    {
                        status = RunStatus.Failed;
                        failure = zero == null
                            ? $"Ended before its first picture: {session.ExitText}"
                            : $"Ended {(now() - zero.Value).TotalSeconds:0} s after its first picture, before the measurement was done: {session.ExitText}";
                        break;
                    }
                    var t = now();
                    if (zero is not DateTime z)
                    {
                        var waited = (t - start).TotalSeconds;
                        if (waited >= firstFrameLimit)
                        {
                            status = RunStatus.Failed;
                            failure = $"No picture within {firstFrameLimit:0} s (warm-up + {o.FirstFrameGraceSeconds:0} s).";
                            break;
                        }
                        Report(BenchPhase.WaitingForPicture, waited, firstFrameLimit);
                        continue;
                    }
                    var since = (t - z).TotalSeconds;
                    if (since >= end) break;
                    if (since < o.WarmupSeconds) Report(BenchPhase.WarmingUp, since, o.WarmupSeconds);
                    else Report(BenchPhase.Measuring, since - o.WarmupSeconds, o.MeasureSeconds);
                }
            }
            catch (OperationCanceledException) { status = RunStatus.Cancelled; failure = "Cancelled."; }

            Report(BenchPhase.Stopping, 0, 0);
            await StopAsync(session, o).ConfigureAwait(false);
            try { Take(); } catch (IOException) { }
            if (zero is DateTime z0) run.FirstFrameSeconds = Math.Max(0, (z0 - start).TotalSeconds);

            if (status == RunStatus.Ok)
            {
                run.Metrics = Measure(frames, lines, zero!.Value, o);
                var last = frames.Count > 0 ? frames[^1].At : 0;
                if (run.Metrics.Frames < 2)
                    (status, failure) = (RunStatus.Failed, "Fewer than two pictures in the measured window.");
                else if (Math.Min(last, end) < end - o.FrozenSeconds)
                    (status, failure) = (RunStatus.Failed, $"No picture after {last:0} s of the frame log ({end:0} s were due): the game froze.");
            }
            return Done(run, status, failure, session);
        }
    }

    /// <summary>The figures of one run over its measured window (frame-log seconds warm-up to warm-up + measured).</summary>
    internal static RunMetrics Measure(IReadOnlyList<FrameSample> frames, IEnumerable<LogLine> lines, DateTime zero, BenchOptions o)
    {
        var from = o.WarmupSeconds;
        var to = o.WarmupSeconds + o.MeasureSeconds;
        var pacing = FramePacing.Of(FramePacing.Window(frames, from, to).Select(f => f.Ms));
        var m = new RunMetrics
        {
            AvgFps = pacing.AvgFps, Low1Fps = pacing.Low1Fps, MedianMs = pacing.MedianMs, WorstMs = pacing.WorstMs,
            Hitches = pacing.Hitches, Frames = pacing.Frames,
        };
        var fps = new List<FpsLine>();
        long? peakPrivate = null, peakVram = null;
        foreach (var line in lines)
        {
            var t = (line.At.ToUniversalTime() - zero).TotalSeconds;
            // The figures of the five-second line are of the second before it; take lines whose second lies in the window.
            if (LayerLines.ParseFps(line.Text) is { } f)
            {
                if (t - 1 >= from && t <= to + Slack) fps.Add(f);
                continue;
            }
            if (t < from || t > to + Slack) continue;
            if (LayerLines.ParseMemory(line.Text) is { } mem)
            {
                m.MemoryLines++;
                if (mem.TryGetValue(LayerLines.PrivateField, out var p)) peakPrivate = Math.Max(peakPrivate ?? 0, p);
                if (mem.TryGetValue(LayerLines.VramField, out var v)) peakVram = Math.Max(peakVram ?? 0, v);
            }
        }
        m.FpsLines = fps.Count;
        if (fps.Count > 0)
        {
            m.PreparingMs = fps.Average(f => f.PreparingMs);
            m.GpuMs = fps.Average(f => f.GpuMs);
            m.CopyBackMs = fps.Average(f => f.CopyBackMs);
        }
        (m.PeakPrivateMb, m.PeakVramMb) = (peakPrivate, peakVram);
        return m;
    }

    /// <summary>How late a line may be read after its moment (stderr is read as it comes; the frame log 4x a second).</summary>
    private const double Slack = 0.5;

    private async Task StopAsync(IBenchSession session, BenchOptions o)
    {
        if (session.State is SessionState.Exited or SessionState.Failed) return;
        await session.StopAsync(TimeSpan.FromSeconds(o.StopGraceSeconds)).ConfigureAwait(false);
        // Never two victory processes: wait until this one is really gone before the next run starts.
        var until = now() + TimeSpan.FromSeconds(o.StopGraceSeconds + 10);
        while (session.State is not (SessionState.Exited or SessionState.Failed) && now() < until)
            await delay(Poll, CancellationToken.None).ConfigureAwait(false);
    }

    private BenchRun Done(BenchRun run, RunStatus status, string? failure, IBenchSession? session = null)
    {
        run.Status = status;
        run.Failure = failure;
        if (session != null)
        {
            run.Problems = session.Problems.Select(p => $"{p.Title}: {p.Explanation}" + (p.Evidence.Length > 0 ? $" ({p.Evidence})" : "")).ToList();
            if (status == RunStatus.Failed && session.ExitText is { } exit && failure?.Contains(exit, StringComparison.Ordinal) != true)
                run.Failure = $"{failure} ({exit})";
        }
        run.Ended = DateTime.Now;
        return run;
    }
}
