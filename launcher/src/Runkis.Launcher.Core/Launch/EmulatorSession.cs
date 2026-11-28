using System.Diagnostics;
using System.Text;
using System.Text.RegularExpressions;

namespace Runkis.Launcher.Core;

public enum SessionState { Starting, Running, Stopping, Exited, Failed }

public sealed record LogLine(DateTime At, bool Error, string Text)
{
    public Severity Level { get; } = Classify(Text, Error);

    // tracing-subscriber's level word comes after the timestamp: "2026-11-27T08:00:00.000Z  WARN victory_gpu::...".
    private static Severity Classify(string t, bool stderr)
    {
        var head = t.Length > 48 ? t[..48] : t;
        if (head.Contains(" ERROR ") || t.StartsWith("Error:", StringComparison.Ordinal)) return Severity.Error;
        if (head.Contains(" WARN ")) return Severity.Warning;
        return Severity.Info;
    }
}

/// <summary>
/// One run of victory.exe: started from a <see cref="LaunchPlan"/>, its output kept in a log file and handed out
/// line by line, stopped by closing its window first and ending it only if that does not work.
/// </summary>
public sealed partial class EmulatorSession : IDisposable
{
    private readonly LaunchPlan plan;
    private readonly StreamWriter log;
    private readonly object logGate = new();
    private readonly Dictionary<string, Problem> problems = [];
    private Process? process;
    private int? adoptedPid;
    private bool stopAsked;

    public event Action<LogLine>? Line;
    public event Action<Problem>? ProblemFound;
    public event Action<SessionState>? StateChanged;

    public SessionState State { get; private set; } = SessionState.Starting;
    public LaunchPlan Plan => plan;
    public DateTime Started { get; private set; }
    public int? ExitCode { get; private set; }
    public string? ExitText { get; private set; }
    /// <summary>The process to measure: the first one, or the one the title relaunched itself as.</summary>
    public int? Pid => adoptedPid ?? (process is { HasExited: false } p ? p.Id : null);
    public IReadOnlyCollection<Problem> Problems { get { lock (problems) return problems.Values.ToList(); } }
    /// <summary>The run was ended because the user asked (Stop): not an error, whatever the exit code.</summary>
    public bool StoppedByUser => stopAsked;

    [GeneratedRegex(@"\x1B\[[0-9;]*[A-Za-z]")]
    private static partial Regex Ansi();

    public EmulatorSession(LaunchPlan plan)
    {
        this.plan = plan;
        Directory.CreateDirectory(Path.GetDirectoryName(plan.LogPath)!);
        log = new StreamWriter(new FileStream(plan.LogPath, FileMode.Create, FileAccess.Write, FileShare.Read), new UTF8Encoding(false)) { AutoFlush = false };
    }

    public void Start()
    {
        WriteHeader();
        if (plan.Victory.EndsWith("victory_sm.exe", StringComparison.OrdinalIgnoreCase)) RefreshSmoothMotionCopy();
        var psi = new ProcessStartInfo(plan.Victory)
        {
            UseShellExecute = false,
            RedirectStandardOutput = true,
            RedirectStandardError = true,
            RedirectStandardInput = false,
            CreateNoWindow = true,
            WorkingDirectory = plan.WorkDir,
            StandardOutputEncoding = Encoding.UTF8,
            StandardErrorEncoding = Encoding.UTF8,
        };
        foreach (var a in plan.Args) psi.ArgumentList.Add(a);
        // Settings left over in the launcher's own environment never reach the run (play-nvidia.ps1 does the same).
        foreach (var key in psi.Environment.Keys.Where(k => k.StartsWith("VICTORY_", StringComparison.OrdinalIgnoreCase)
                     || k.StartsWith("VN_", StringComparison.OrdinalIgnoreCase)).ToList())
            psi.Environment.Remove(key);
        foreach (var (k, v) in plan.Settings) psi.Environment[k] = v;

        var p = new Process { StartInfo = psi, EnableRaisingEvents = true };
        p.OutputDataReceived += (_, e) => { if (e.Data != null) OnLine(e.Data, false); };
        p.ErrorDataReceived += (_, e) => { if (e.Data != null) OnLine(e.Data, true); };
        p.Exited += (_, _) => OnExited(p);
        try
        {
            if (!p.Start()) throw new InvalidOperationException("the process did not start");
        }
        catch (Exception e)
        {
            ExitText = $"Could not start {plan.Victory}: {e.Message}";
            WriteLog($"[launcher] {ExitText}");
            SetState(SessionState.Failed);
            FlushLog();
            return;
        }
        process = p;
        Started = DateTime.Now;
        p.BeginOutputReadLine();
        p.BeginErrorReadLine();
        WriteLog($"[launcher] started process {p.Id}");
        SetState(SessionState.Running);
    }

    private void WriteHeader()
    {
        WriteLog($"[launcher] {DateTime.Now:yyyy-MM-dd HH:mm:ss} {plan.Mode}");
        WriteLog($"[launcher] {plan.CommandLine}");
        WriteLog($"[launcher] working folder {plan.WorkDir}");
        foreach (var (k, v) in plan.Settings.OrderBy(kv => kv.Key, StringComparer.Ordinal))
            WriteLog($"[launcher]   {k}={(v.Length > 120 ? v[..120] + $"... ({v.Length} chars)" : v)}");
        if (plan.Overridden.Count > 0) WriteLog($"[launcher] set by the profile over the script: {string.Join(", ", plan.Overridden)}");
    }

    /// <summary>A copy under the name an NVIDIA App Smooth Motion profile can be set for, refreshed when the build changes.</summary>
    private void RefreshSmoothMotionCopy()
    {
        var source = Path.Combine(Path.GetDirectoryName(plan.Victory)!, "victory.exe");
        try
        {
            if (File.Exists(source) && (!File.Exists(plan.Victory) || File.GetLastWriteTimeUtc(plan.Victory) < File.GetLastWriteTimeUtc(source)))
            {
                File.Copy(source, plan.Victory, overwrite: true);
                WriteLog("[launcher] refreshed victory_sm.exe from victory.exe");
            }
        }
        catch (IOException e) { WriteLog($"[launcher] could not refresh victory_sm.exe: {e.Message}"); }
    }

    private void OnLine(string raw, bool stderr)
    {
        var text = Ansi().Replace(raw, "");
        WriteLog(text);
        var line = new LogLine(DateTime.Now, stderr, text);
        Line?.Invoke(line);
        if (LogAnalyzer.Match(text) is { } problem)
        {
            bool fresh;
            lock (problems) fresh = problems.TryAdd(problem.Id, problem);
            if (fresh) ProblemFound?.Invoke(problem);
        }
    }

    private void OnExited(Process p)
    {
        // Let the output readers finish before the log is closed.
        try { p.WaitForExit(); } catch (InvalidOperationException) { }
        int code;
        try { code = p.ExitCode; } catch (InvalidOperationException) { code = -1; }
        // A title that restarts itself starts a new victory process and the first one ends with 0
        // (victory-hle lifecycle.rs, VICTORY_RELAUNCHED_FROM), and so do victory play's restarts (the first start's
        // warm-up and the F1 menu's "save and restart", through PowerShell in a new console: menu.rs `restart`);
        // follow it instead of calling the run over. Its output goes to its own console then, not to this log.
        if (code == 0 && !stopAsked && FindRelaunch(p.Id) is int child)
        {
            WriteLog($"[launcher] the title restarted itself as process {child}; following it");
            adoptedPid = child;
            _ = WatchAdopted(child);
            return;
        }
        Finish(code);
    }

    private static int? FindRelaunch(int pid)
    {
        // The PowerShell that starts victory play again may take a moment to.
        for (var tries = 0; tries < 20; tries++)
        {
            foreach (var (child, exe) in Native.ChildrenOf(pid))
            {
                if (exe.StartsWith("victory", StringComparison.OrdinalIgnoreCase)) return child;
                if (exe.StartsWith("powershell", StringComparison.OrdinalIgnoreCase))
                    foreach (var (grandchild, name) in Native.ChildrenOf(child))
                        if (name.StartsWith("victory", StringComparison.OrdinalIgnoreCase)) return grandchild;
            }
            if (!Native.ChildrenOf(pid).Any(c => c.Item2.StartsWith("powershell", StringComparison.OrdinalIgnoreCase)))
                return null;
            Thread.Sleep(250);
        }
        return null;
    }

    private async Task WatchAdopted(int pid)
    {
        try
        {
            using var p = Process.GetProcessById(pid);
            await p.WaitForExitAsync().ConfigureAwait(false);
            // Not our child: Windows gives its exit code only to a handle opened before it ended.
            int code;
            try { code = p.ExitCode; } catch (InvalidOperationException) { code = 0; }
            Finish(code);
        }
        catch (ArgumentException) { Finish(0); }
    }

    private void Finish(int code)
    {
        ExitCode = code;
        ExitText = stopAsked && code != 0 ? "Stopped by the launcher." : LogAnalyzer.DescribeExit(code);
        WriteLog($"[launcher] exited with {code} (0x{unchecked((uint)code):X8}): {ExitText}");
        foreach (var crash in CrashLogs())
        {
            WriteLog($"[launcher] crash log written during the run: {crash}");
            foreach (var l in ReadTail(crash, 40)) OnLine(l, true);
        }
        FlushLog();
        SetState(SessionState.Exited);
    }

    /// <summary>crashes.log files (data\&lt;title id&gt;\crashes.log) changed since the run began.</summary>
    public IEnumerable<string> CrashLogs()
    {
        if (!Directory.Exists(plan.DataDir)) return [];
        try
        {
            return Directory.EnumerateFiles(plan.DataDir, "crashes.log", new EnumerationOptions { RecurseSubdirectories = true, MaxRecursionDepth = 2 })
                .Where(f => File.GetLastWriteTime(f) >= Started.AddSeconds(-1)).ToList();
        }
        catch (IOException) { return []; }
        catch (UnauthorizedAccessException) { return []; }
    }

    private static IEnumerable<string> ReadTail(string path, int lines)
    {
        try
        {
            using var fs = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite);
            using var r = new StreamReader(fs);
            return r.ReadToEnd().Split('\n').Select(l => l.TrimEnd('\r')).TakeLast(lines).ToList();
        }
        catch (IOException) { return []; }
    }

    /// <summary>Asks the game's window to close (the title can save), then ends the process after <paramref name="grace"/>.</summary>
    public async Task StopAsync(TimeSpan grace)
    {
        if (State is SessionState.Exited or SessionState.Failed) return;
        stopAsked = true;
        SetState(SessionState.Stopping);
        var pid = Pid;
        if (pid == null) return;
        WriteLog("[launcher] stop: asking the game's window to close");
        foreach (var h in Native.WindowsOf(pid.Value)) Native.PostMessage(h, Native.WM_CLOSE, IntPtr.Zero, IntPtr.Zero);
        try
        {
            using var p = Process.GetProcessById(pid.Value);
            using var cts = new CancellationTokenSource(grace);
            try { await p.WaitForExitAsync(cts.Token).ConfigureAwait(false); }
            catch (OperationCanceledException)
            {
                WriteLog($"[launcher] stop: still running after {grace.TotalSeconds:0} s; ending the process");
                p.Kill(entireProcessTree: true);
            }
        }
        catch (ArgumentException) { /* already gone */ }
        catch (InvalidOperationException) { }
    }

    private void SetState(SessionState s) { State = s; StateChanged?.Invoke(s); }

    private void WriteLog(string text)
    {
        lock (logGate)
        {
            try { log.WriteLine(text); } catch (ObjectDisposedException) { }
        }
    }

    private void FlushLog() { lock (logGate) { try { log.Flush(); } catch (ObjectDisposedException) { } } }

    /// <summary>Writes buffered log lines out (called by the UI's timer, so a crash loses at most a second).</summary>
    public void Flush() => FlushLog();

    public void Dispose()
    {
        FlushLog();
        lock (logGate) log.Dispose();
        process?.Dispose();
    }

    /// <summary>Keeps the newest <paramref name="keep"/> sessions' logs and frame logs.</summary>
    public static void PruneLogs(string folder, int keep = 30)
    {
        if (!Directory.Exists(folder)) return;
        var sessions = Directory.EnumerateFiles(folder, "*.log").OrderByDescending(File.GetLastWriteTimeUtc).Skip(keep);
        foreach (var f in sessions)
        {
            try
            {
                File.Delete(f);
                var frames = Path.ChangeExtension(f, ".frames.txt");
                if (File.Exists(frames)) File.Delete(frames);
            }
            catch (IOException) { }
        }
    }
}
