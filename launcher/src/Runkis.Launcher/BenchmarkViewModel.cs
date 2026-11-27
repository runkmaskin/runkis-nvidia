using System.Collections.ObjectModel;
using System.Globalization;
using System.IO;
using System.Windows;
using System.Windows.Data;
using System.Windows.Input;
using System.Windows.Media;
using Microsoft.Win32;
using Runkis.Launcher.Core;

namespace Runkis.Launcher;

/// <summary>A finished or running run, as the page lists it.</summary>
public sealed record RunRow(string Title, string Status, string Detail, Brush Tone, string? LogPath);

/// <summary>The Benchmark page. The work is <see cref="BenchmarkRunner"/>'s; this only collects the input and shows the result.</summary>
public sealed class BenchmarkViewModel : Observable
{
    private readonly MainViewModel main;
    private readonly BenchmarkStore store = new(AppPaths.Benchmarks);
    private CancellationTokenSource? cts;
    private Task? runTask;
    private BenchResult? shown;

    public BenchmarkViewModel(MainViewModel main)
    {
        this.main = main;
        Add = new Command(DoAdd, () => !Running && SelectedSource != null);
        Remove = new Command(p => { if (p is BenchCandidate c) Candidates.Remove(c); }, _ => !Running);
        MakeBaseline = new Command(p => { if (p is BenchCandidate c && Candidates.IndexOf(c) > 0) Candidates.Move(Candidates.IndexOf(c), 0); }, _ => !Running);
        Start = new Command(() => _ = StartAsync(), () => !Running && Candidates.Count > 0 && !main.EmulatorBusy && main.Repo != null);
        Cancel = new Command(() => { cts?.Cancel(); ProgressText = "Cancelling: stopping the current run…"; }, () => Running);
        LoadKeys = new Command(DoLoadKeys, () => !Running);
        SaveKeys = new Command(DoSaveKeys);
        ExportCsv = new Command(DoExportCsv, () => shown != null);
        OpenFolder = new Command(() => MainViewModel.OpenFolder(AppPaths.Benchmarks));
        OpenRunLog = new Command(p => MainViewModel.OpenFile(p as string), p => p is string s && File.Exists(s));
        Refresh = new Command(() => { RefreshSources(); RefreshPast(); });
        Candidates.CollectionChanged += (_, _) => Raise(nameof(PlanText));
        RefreshSources();
        RefreshPast();
        CheckKeys();
    }

    // ---------------------------------------------------------------- candidates
    public ObservableCollection<Choice> Sources { get; } = [];
    private Choice? selectedSource;
    public Choice? SelectedSource { get => selectedSource; set => Set(ref selectedSource, value); }
    private string extraText = "";
    /// <summary>Raw VICTORY_X=Y lines for the next candidate.</summary>
    public string ExtraText { get => extraText; set { if (Set(ref extraText, value)) ExtraErrors = ""; } }
    private string extraErrors = "";
    public string ExtraErrors { get => extraErrors; private set => Set(ref extraErrors, value); }
    public ObservableCollection<BenchCandidate> Candidates { get; } = [];

    /// <summary>The presets (with the one worked out for this PC) and the saved profiles.</summary>
    public void RefreshSources()
    {
        var keep = SelectedSource?.Label;
        Sources.Clear();
        foreach (var c in main.StartChoices.Where(c => c.Value is Preset)) Sources.Add(new($"Preset: {c.Label}", c.Value));
        foreach (var n in main.ProfileNames) Sources.Add(new($"Profile: {n}", n));
        SelectedSource = Sources.FirstOrDefault(s => s.Label == keep) ?? Sources.FirstOrDefault();
    }

    private void DoAdd()
    {
        if (SelectedSource == null) return;
        var extra = BenchCandidate.ParseExtra(ExtraText, out var errors);
        if (errors.Count > 0) { ExtraErrors = string.Join("\n", errors); return; }
        LaunchProfile profile;
        string name, source;
        CandidateKind kind;
        try
        {
            switch (SelectedSource.Value)
            {
                case Preset p:
                    (profile, name, source, kind) = (Presets.Make(p, main.Profile), p.Name, p.Id, CandidateKind.Preset);
                    break;
                case string n:
                    (profile, name, source, kind) = (main.Store.Load(n), n, n, CandidateKind.Profile);
                    break;
                default: return;
            }
        }
        catch (Exception e) when (e is IOException or InvalidDataException) { main.Notify($"Could not open the profile: {e.Message}"); return; }
        var blocking = ProfileValidator.Validate(profile, main.Repo).Where(i => i.Blocking).ToList();
        if (blocking.Count > 0) { ExtraErrors = "Not added: " + string.Join(" ", blocking.Select(i => i.Message)); return; }
        if (extra.Count > 0) name += " + " + string.Join(" ", extra.Select(kv => $"{kv.Key["VICTORY_".Length..]}={kv.Value}"));
        var unique = name;
        for (var i = 2; Candidates.Any(c => string.Equals(c.Name, unique, StringComparison.OrdinalIgnoreCase)); i++) unique = $"{name} ({i})";
        profile.Name = unique;
        Candidates.Add(new BenchCandidate { Name = unique, Kind = kind, Source = source, Profile = profile, Extra = extra });
        ExtraText = "";
        CommandManager.InvalidateRequerySuggested();
    }

    // ---------------------------------------------------------------- options and key script
    private string warmupText = "40", measureText = "90", repeatsText = "2", keysText = "";
    public string WarmupText { get => warmupText; set { if (Set(ref warmupText, value)) { CheckKeys(); Raise(nameof(PlanText)); } } }
    public string MeasureText { get => measureText; set { if (Set(ref measureText, value)) { CheckKeys(); Raise(nameof(PlanText)); } } }
    public string RepeatsText { get => repeatsText; set { if (Set(ref repeatsText, value)) Raise(nameof(PlanText)); } }
    /// <summary>The key script as typed: entries on lines or comma-separated, # comments.</summary>
    public string KeysText { get => keysText; set { if (Set(ref keysText, value)) CheckKeys(); } }
    private string keysCheck = "";
    public string KeysCheck { get => keysCheck; private set => Set(ref keysCheck, value); }
    private Brush keysTone = Brushes.Gray;
    public Brush KeysTone { get => keysTone; private set => Set(ref keysTone, value); }

    private static double? Number(string s) =>
        double.TryParse(s.Trim().Replace(',', '.'), NumberStyles.Float, CultureInfo.InvariantCulture, out var v) ? v : null;

    private BenchOptions? Options(out string error)
    {
        error = "";
        var o = new BenchOptions { Keys = KeyScript.ToVariable(KeysText) };
        if (Number(WarmupText) is not double w) { error = "The warm-up is not a number of seconds."; return null; }
        if (Number(MeasureText) is not double m) { error = "The measured time is not a number of seconds."; return null; }
        if (!int.TryParse(RepeatsText.Trim(), NumberStyles.Integer, CultureInfo.InvariantCulture, out var r)) { error = "Repeats is not a whole number."; return null; }
        (o.WarmupSeconds, o.MeasureSeconds, o.Repeats) = (w, m, r);
        var problems = o.Validate();
        if (problems.Count > 0) { error = string.Join(" ", problems); return null; }
        return o;
    }

    public string PlanText
    {
        get
        {
            var o = Options(out var error);
            if (o == null) return error;
            var runs = Candidates.Count * o.Repeats;
            var each = o.WarmupSeconds + o.MeasureSeconds;
            return $"{runs} run(s) of {each:0} s after the first picture each, plus start-up and stop: at least {runs * each / 60:0.#} min. "
                   + "Order: the candidates in turn (A B A B), so drift over the session averages out.";
        }
    }

    private void CheckKeys()
    {
        var warm = Number(WarmupText) ?? 40;
        var measure = Number(MeasureText) ?? 90;
        var value = KeyScript.ToVariable(KeysText);
        if (value.Length == 0)
        {
            KeysCheck = "No key script: the run gets no scripted input (VICTORY_KEYS is not set).";
            KeysTone = (Brush)Application.Current.Resources["Faint"];
            return;
        }
        var c = KeyScript.Check(value, warm, measure);
        var lines = c.Errors.Select(e => "✕ " + e).Concat(c.Warnings.Select(w => "! " + w)).Concat(c.Notes).ToList();
        lines.Add($"Passed as VICTORY_KEYS={value}");
        KeysCheck = string.Join("\n", lines);
        KeysTone = (Brush)Application.Current.Resources[c.Errors.Count > 0 ? "Bad" : c.Warnings.Count > 0 ? "Warn" : "Muted"];
    }

    private void DoLoadKeys()
    {
        var dlg = new OpenFileDialog { Filter = "Key script (*.txt)|*.txt|All files|*.*", Title = "Load a key script" };
        if (dlg.ShowDialog() != true) return;
        try { KeysText = File.ReadAllText(dlg.FileName); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { main.Notify($"Not loaded: {e.Message}"); }
    }

    private void DoSaveKeys()
    {
        var dlg = new SaveFileDialog { Filter = "Key script (*.txt)|*.txt", FileName = "keys.txt", Title = "Save the key script" };
        if (dlg.ShowDialog() != true) return;
        try { File.WriteAllText(dlg.FileName, KeysText); main.Notify("Saved."); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { main.Notify($"Not saved: {e.Message}"); }
    }

    // ---------------------------------------------------------------- running
    private bool running;
    public bool Running
    {
        get => running;
        private set { if (Set(ref running, value)) { main.BenchChanged(); CommandManager.InvalidateRequerySuggested(); } }
    }
    private string progressText = "";
    public string ProgressText { get => progressText; private set => Set(ref progressText, value); }
    private double progressValue;
    public double ProgressValue { get => progressValue; private set => Set(ref progressValue, value); }
    public ObservableCollection<RunRow> Runs { get; } = [];

    private async Task StartAsync()
    {
        if (Running || main.Repo is not RepoLayout repo) return;
        var o = Options(out var error);
        if (o == null) { main.Notify(error); return; }
        var keys = KeyScript.Check(o.Keys, o.WarmupSeconds, o.MeasureSeconds);
        if (!keys.Ok) { main.Notify("The key script has entries the layer would skip: " + string.Join(" ", keys.Errors)); return; }

        var candidates = Candidates.Select(c => c).ToList();
        Running = true;
        cts = new CancellationTokenSource();
        Runs.Clear();
        Show(null);
        ProgressValue = 0;
        ProgressText = "Starting…";
        main.ShowState("Benchmarking", "See the Benchmark page.", "Accent", $"Benchmark · {candidates.Count} candidate(s)");
        var dispatcher = Application.Current.Dispatcher;
        var runner = new BenchmarkRunner(EmulatorBenchSession.Factory(repo));
        var span = o.WarmupSeconds + o.MeasureSeconds;
        runner.Progress += p => dispatcher.BeginInvoke(() =>
        {
            if (p.Phase == BenchPhase.Done) return;
            ProgressText = p.Text;
            var within = p.Phase switch
            {
                BenchPhase.WarmingUp => p.Seconds / span,
                BenchPhase.Measuring => (o.WarmupSeconds + p.Seconds) / span,
                BenchPhase.Stopping => 1,
                _ => 0,
            };
            ProgressValue = (p.Run - 1 + Math.Clamp(within, 0, 1)) / Math.Max(1, p.Runs);
        });
        runner.RunFinished += r => dispatcher.BeginInvoke(() => Runs.Add(Row(r, candidates)));
        try
        {
            var task = Task.Run(() => runner.RunAsync(candidates, o, main.MachineText, cts.Token));
            runTask = task;
            var result = await task;
            try { store.Save(result); }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException) { main.Notify($"The result was not saved: {e.Message}"); }
            Show(result);
            RefreshPast();
            var ok = result.Runs.Count(r => r.Status == RunStatus.Ok);
            ProgressText = (result.Cancelled ? "Cancelled. " : "Done. ") + $"{ok} of {result.Runs.Count} runs good; saved as {Path.GetFileName(result.Path) ?? "—"}.";
            ProgressValue = 1;
            main.ShowState(result.Cancelled ? "Benchmark cancelled" : "Benchmark done", ProgressText, ok == result.Runs.Count ? "Muted" : "Warn", "");
        }
        catch (ArgumentException e) { ProgressText = e.Message; main.ShowState("Benchmark not started", e.Message, "Bad", ""); }
        finally
        {
            runTask = null;
            cts.Dispose();
            cts = null;
            Running = false;
        }
    }

    /// <summary>For closing the window: cancels and waits for the current run to be stopped.</summary>
    public void CancelAndWait(TimeSpan patience)
    {
        cts?.Cancel();
        try { runTask?.Wait(patience); }
        catch (AggregateException) { }
    }

    private static RunRow Row(BenchRun r, IReadOnlyList<BenchCandidate> candidates)
    {
        var m = r.Metrics;
        var name = r.Candidate < candidates.Count ? candidates[r.Candidate].Name : $"#{r.Candidate}";
        var title = $"{r.Order + 1}. {name} · repeat {r.Repeat + 1}";
        var tone = (Brush)Application.Current.Resources[r.Status == RunStatus.Ok ? "Good" : r.Status == RunStatus.Failed ? "Bad" : "Warn"];
        string detail;
        if (r.Status == RunStatus.Ok)
            detail = $"{BenchStats.Num(m.AvgFps, "0.0")} fps · 1 % low {BenchStats.Num(m.Low1Fps, "0.0")} · median {BenchStats.Num(m.MedianMs, "0.0")} ms · "
                     + $"{(m.Hitches is int h ? h.ToString(CultureInfo.InvariantCulture) : "—")} hitches · worst {BenchStats.Num(m.WorstMs, "0")} ms · {m.Frames} frames · "
                     + $"first picture after {BenchStats.Num(r.FirstFrameSeconds, "0")} s"
                     + (m.FpsLines == 0 ? " · no five-second lines in the window" : "")
                     + (r.Restarts > 0 ? $" · the title restarted itself {r.Restarts}x" : "");
        else
            detail = (r.Failure ?? "") + (r.Problems.Count > 0 ? "\n" + string.Join("\n", r.Problems) : "");
        return new(title, r.Status.ToString(), detail, tone, r.LogPath);
    }

    // ---------------------------------------------------------------- results
    private BenchReport? report;
    public BenchReport? Report { get => report; private set => Set(ref report, value); }
    private string resultTitle = "No benchmark yet";
    public string ResultTitle { get => resultTitle; private set => Set(ref resultTitle, value); }

    private void Show(BenchResult? r)
    {
        shown = r;
        Report = r == null ? null : BenchReport.Of(r);
        ResultTitle = r == null ? "No benchmark yet"
            : $"{r.Started:yyyy-MM-dd HH:mm} · {r.Options.WarmupSeconds:0} s warm-up, {r.Options.MeasureSeconds:0} s measured, {r.Options.Repeats} repeat(s)"
              + (r.Options.Keys.Length > 0 ? " · key script" : "") + (r.Cancelled ? " · cancelled" : "") + (r.Machine.Length > 0 ? $" · {r.Machine}" : "");
        if (r != null && !Running)
        {
            Runs.Clear();
            foreach (var run in r.Runs) Runs.Add(Row(run, r.Candidates));
        }
        CommandManager.InvalidateRequerySuggested();
    }

    public ObservableCollection<BenchEntry> Past { get; } = [];
    private BenchEntry? selectedPast;
    public BenchEntry? SelectedPast
    {
        get => selectedPast;
        set
        {
            if (!Set(ref selectedPast, value) || value == null || Running) return;
            try { Show(BenchmarkStore.Load(value.Path)); }
            catch (Exception e) when (e is IOException or InvalidDataException or UnauthorizedAccessException) { main.Notify($"Could not open it: {e.Message}"); }
        }
    }

    private void RefreshPast()
    {
        Past.Clear();
        foreach (var e in store.List()) Past.Add(e);
    }

    private void DoExportCsv()
    {
        if (shown == null) return;
        var dlg = new SaveFileDialog
        {
            Filter = "CSV (*.csv)|*.csv",
            FileName = $"benchmark-{shown.Started.ToString("yyyyMMdd-HHmmss", CultureInfo.InvariantCulture)}.csv",
            Title = "Export the benchmark",
        };
        if (dlg.ShowDialog() != true) return;
        try { BenchmarkStore.ExportCsv(shown, dlg.FileName); main.Notify("Exported."); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { main.Notify($"Not exported: {e.Message}"); }
    }

    public Command Add { get; }
    public Command Remove { get; }
    public Command MakeBaseline { get; }
    public Command Start { get; }
    public Command Cancel { get; }
    public Command LoadKeys { get; }
    public Command SaveKeys { get; }
    public Command ExportCsv { get; }
    public Command OpenFolder { get; }
    public Command OpenRunLog { get; }
    public Command Refresh { get; }
}

/// <summary>A table cell's colour: better beyond noise green, worse red, everything else muted.</summary>
public sealed class CellTone : IValueConverter
{
    public object Convert(object value, Type t, object p, CultureInfo c) => value switch
    {
        BenchCell { Better: true } => Application.Current.Resources["Good"],
        BenchCell { Better: false } => Application.Current.Resources["Bad"],
        _ => Application.Current.Resources["Faint"],
    };

    public object ConvertBack(object value, Type t, object p, CultureInfo c) => throw new NotSupportedException();
}
