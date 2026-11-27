using System.Collections.Concurrent;
using System.Collections.ObjectModel;
using System.ComponentModel;
using System.Diagnostics;
using System.IO;
using System.Text;
using System.Windows;
using System.Windows.Data;
using System.Windows.Input;
using System.Windows.Media;
using System.Windows.Threading;
using Microsoft.Win32;
using Runkis.Launcher.Core;

namespace Runkis.Launcher;

public sealed record Choice(string Label, object? Value)
{
    public override string ToString() => Label;
}

public sealed class GpuRow(GpuAdapter adapter) : Observable
{
    private string live = "";
    public GpuAdapter Adapter { get; } = adapter;
    public string Title => $"#{Adapter.Index}  {Adapter.Name}";
    public string Details => $"{Adapter.Vendor} · {Adapter.DedicatedVideoMemory / 1073741824.0:0.#} GB dedicated · driver "
                             + (Adapter.NvidiaVersion is { } nv ? $"{nv} ({Adapter.DriverVersion})" : Adapter.DriverVersion ?? "unknown")
                             + (Adapter.Software ? " · software" : "");
    public string Live { get => live; set => Set(ref live, value); }
}

public sealed class MainViewModel : Observable, IDisposable
{
    private const double MB = 1048576;
    private readonly LauncherSettings settings = LauncherSettings.Load();
    private readonly ProfileStore store = new(AppPaths.Profiles);
    private readonly DispatcherTimer fast, slow;
    private readonly ConcurrentQueue<LogLine> pending = new();
    private readonly FrameStats frames = new();
    private readonly GpuCounters gpuCounters = new();
    private readonly CpuMeter cpu = new();
    private readonly Nvml nvml = new();
    private readonly List<GpuAdapter> adapters;
    private RepoLayout? repo;
    private LaunchProfile profile = new();
    private EmulatorSession? session;
    private FileTail? frameTail;
    private TimeSpan lastCpuTime;
    private DateTime lastCpuAt;
    private int? cpuPid;
    private bool sampling;
    private string page = "Play";

    public MainViewModel()
    {
        adapters = Dxgi.Adapters();
        Gpus = new(adapters.Select(a => new GpuRow(a)));
        AdapterChoices = [new("Windows' default card (#0)", "")];
        foreach (var a in adapters.Where(a => !a.Software)) AdapterChoices.Add(new($"#{a.Index}  {a.Name}", $"#{a.Index}"));
        ResolutionChoices = [new("Half of the output (default)", null), .. ProfileValidator.Resolutions.Select(r => new Choice($"{r}p", (int?)r))];

        repo = RepoLayout.Find(settings.RepoRoot) ?? RepoLayout.Find(AppContext.BaseDirectory) ?? RepoLayout.Find(Environment.CurrentDirectory);
        LogView = CollectionViewSource.GetDefaultView(LogLines);
        LogView.Filter = o => o is LogLine l && (!ErrorsOnly || l.Level != Severity.Info)
                              && (LogSearch.Length == 0 || l.Text.Contains(LogSearch, StringComparison.OrdinalIgnoreCase));
        LoadProfiles();
        Machine = SystemInfo.Read();
        Bench = new BenchmarkViewModel(this);

        Launch = new Command(() => _ = StartAsync(RunMode.Play), () => CanStart);
        Warm = new Command(() => _ = StartAsync(RunMode.Warm), () => CanStart);
        Precompile = new Command(() => _ = StartAsync(RunMode.Precompile), () => CanStart);
        Stop = new Command(() => _ = StopAsync(), () => IsRunning);
        BrowseGame = new Command(DoBrowseGame);
        BrowseRepo = new Command(DoBrowseRepo);
        BrowseCache = new Command(() => PickFolder(f => profile.CacheDir = f));
        BrowseData = new Command(() => PickFolder(f => profile.DataDir = f));
        ResetFolders = new Command(() => { profile.CacheDir = null; profile.DataDir = null; ProfileChanged(); });
        NewProfile = new Command(() => { var p = new LaunchProfile { Name = store.FreeName("New profile"), GameExe = profile.GameExe }; SwitchTo(p); });
        Duplicate = new Command(() => { var p = profile.Clone(); p.Name = store.FreeName(profile.Name + " copy"); SwitchTo(p); });
        Save = new Command(DoSave);
        Delete = new Command(DoDelete, () => store.Exists(profile.Name));
        Import = new Command(DoImport);
        Export = new Command(DoExport);
        RunChecks = new Command(() => _ = RunChecksAsync(), () => !ChecksRunning);
        Preview = new Command(() => _ = PreviewAsync());
        OpenLogs = new Command(() => OpenFolder(AppPaths.Logs));
        OpenSessionLog = new Command(() => OpenFile(session?.Plan.LogPath), () => session != null);
        OpenCrashLog = new Command(() => OpenFile(session?.CrashLogs().FirstOrDefault()), () => session?.CrashLogs().Any() == true);
        CopyLog = new Command(() => Clipboard.SetText(string.Join(Environment.NewLine, LogView.Cast<LogLine>().Select(l => l.Text))));
        ClearLog = new Command(() => LogLines.Clear());
        Go = new Command(p => Page = p as string ?? "Play");
        SavePreset = new Command(DoSavePreset, () => UsingPreset);
        WarmUp = new Command(() => { warmUpNext = true; _ = StartAsync(RunMode.Precompile); }, () => CanStart);

        fast = new DispatcherTimer(DispatcherPriority.Background) { Interval = TimeSpan.FromMilliseconds(250) };
        fast.Tick += (_, _) => FastTick();
        fast.Start();
        slow = new DispatcherTimer(DispatcherPriority.Background) { Interval = TimeSpan.FromSeconds(1) };
        slow.Tick += (_, _) => _ = SlowTick();
        slow.Start();
        foreach (var t in Tiles) t.Unknown("Not running");
        _ = RunChecksAsync();
        EmulatorSession.PruneLogs(AppPaths.Logs);
    }

    // ---------------------------------------------------------------- navigation and state
    public string Page { get => page; set { if (Set(ref page, value)) { Raise(nameof(PageIndex)); if (value == "Benchmark") Bench.RefreshSources(); } } }
    public int PageIndex => Page switch { "Settings" => 1, "Diagnostics" => 2, "Logs" => 3, "System" => 4, "Benchmark" => 5, _ => 0 };
    public string RepoText => repo?.Root ?? "Not found: choose the runkis-nvidia folder";
    public bool RepoFound => repo != null;
    public SystemSnapshot Machine { get; private set; }

    private string state = "Ready";
    public string State { get => state; private set => Set(ref state, value); }
    private string stateDetail = "";
    public string StateDetail { get => stateDetail; private set => Set(ref stateDetail, value); }
    private Brush stateTone = Brushes.Gray;
    public Brush StateTone { get => stateTone; private set => Set(ref stateTone, value); }
    private bool busy;
    public bool Busy { get => busy; private set { Set(ref busy, value); Raise(nameof(CanStart)); } }
    public bool IsRunning => session is { State: SessionState.Running or SessionState.Starting or SessionState.Stopping };
    public bool CanStart => !Busy && !IsRunning && !Bench.Running && repo != null;
    public BenchmarkViewModel Bench { get; }
    private string runTitle = "";
    public string RunTitle { get => runTitle; private set => Set(ref runTitle, value); }
    /// <summary>The warm-up's progress, read from victory.exe's lines; null outside a warm-up.</summary>
    private WarmProgress? warm;
    private double? warmPercent;
    /// <summary>0..100 while a warm-up's total is known, else null (the bar is hidden).</summary>
    public double? WarmPercent { get => warmPercent; private set { if (Set(ref warmPercent, value)) Raise(nameof(WarmBarVisible)); } }
    public bool WarmBarVisible => warmPercent != null;

    // ---------------------------------------------------------------- profile
    public ObservableCollection<string> ProfileNames { get; } = [];
    public LaunchProfile Profile => profile;
    private string? selectedProfile;
    public string? SelectedProfile
    {
        get => selectedProfile;
        set
        {
            if (!Set(ref selectedProfile, value) || value == null || value == profile.Name && store.Exists(value)) return;
            try { SwitchTo(store.Load(value)); }
            catch (Exception e) when (e is IOException or InvalidDataException) { Toast($"Could not open the profile: {e.Message}"); }
        }
    }
    public string GameName => string.IsNullOrWhiteSpace(profile.GameExe) ? "No game chosen" : Path.GetFileName(profile.GameExe);
    public string GameFolder => string.IsNullOrWhiteSpace(profile.GameExe) ? "Choose your own copy of game_scarlett_master.exe" : Path.GetDirectoryName(profile.GameExe) ?? "";
    public string ProfileSummary => Summary(EffectiveProfile);
    private string Summary(LaunchProfile profile) =>
        $"Series {(profile.Console == ConsoleModel.SeriesX ? "X" : "S")} · {(profile.Resolution is int r ? $"{r}p" : "half scale")} · "
        + $"{(profile.Quality ? "Quality" : "Speed")} · {RepoLayout.BuildFolder(profile.Build)} build · "
        + (Dxgi.Resolve(adapters, profile.Adapter).Adapter?.Name ?? "no card");
    public string ValidationText => string.Join("\n", ProfileValidator.Validate(profile, repo).Select(i => (i.Blocking ? "✕ " : "! ") + i.Message));

    public Array Consoles { get; } = Enum.GetValues<ConsoleModel>();
    public Array Builds { get; } = Enum.GetValues<BuildKind>();
    public Array DredModes { get; } = Enum.GetValues<DredMode>();
    public List<Choice> ResolutionChoices { get; }
    public List<Choice> AdapterChoices { get; }

    // ---------------------------------------------------------------- presets
    /// <summary>"Start as": the selected profile, or a preset over it (game, folders, card and build kept).</summary>
    public List<Choice> StartChoices => startChoices ??=
        [new("My profile", null), new("Recommended for this PC", Presets.ForThisPc(adapters, Machine, null)),
         .. Presets.All.Select(p => new Choice(p.Name, p))];
    private List<Choice>? startChoices;
    private Choice? selectedStart;
    public Choice? SelectedStart
    {
        // The preset worked out for this PC unless the user picks otherwise.
        get => selectedStart ??= StartChoices[1];
        set
        {
            if (!Set(ref selectedStart, value)) return;
            Raise(nameof(StartDescription));
            Raise(nameof(ProfileSummary));
            Raise(nameof(UsingPreset));
            _ = RunChecksAsync();
        }
    }
    public bool UsingPreset => SelectedStart?.Value is Preset;
    public string StartDescription => SelectedStart?.Value is Preset p
        ? p.Description
        : $"Your profile '{profile.Name}', as set on the Settings page.";
    /// <summary>What a run would start with now.</summary>
    public LaunchProfile EffectiveProfile => SelectedStart?.Value is Preset p ? Presets.Make(p, profile) : profile;

    private void DoSavePreset()
    {
        if (SelectedStart?.Value is not Preset preset) return;
        var p = Presets.Make(preset, profile);
        p.Name = store.FreeName(preset.Name);
        try
        {
            store.Save(p);
            ProfileNames.Add(p.Name);
            SwitchTo(p);
            SelectedStart = StartChoices[0];
            Toast($"Saved as the profile '{p.Name}'; change it on the Settings page.");
        }
        catch (Exception e) when (e is IOException or ArgumentException or InvalidDataException or UnauthorizedAccessException) { Toast($"Not saved: {e.Message}"); }
    }

    private string previewText = "Press 'Show effective settings' to see exactly what the next run gets.";
    public string PreviewText { get => previewText; private set => Set(ref previewText, value); }

    /// <summary>Call after the profile object changed outside a binding, so every field shows it.</summary>
    public void ProfileChanged()
    {
        Raise(nameof(Profile));
        Raise(nameof(GameName));
        Raise(nameof(GameFolder));
        Raise(nameof(ProfileSummary));
        Raise(nameof(ValidationText));
        Raise(nameof(StartDescription));
    }

    private void LoadProfiles()
    {
        ProfileNames.Clear();
        foreach (var n in store.List()) ProfileNames.Add(n);
        if (ProfileNames.Count == 0)
        {
            // The first profile is PLAY.bat's set, so the launcher starts where the tuned script does.
            var first = new LaunchProfile { Name = "RTX 2070 SUPER tuned (PLAY.bat)" };
            store.Save(first);
            ProfileNames.Add(first.Name);
        }
        var name = settings.LastProfile != null && ProfileNames.Contains(settings.LastProfile) ? settings.LastProfile : ProfileNames[0];
        try { SwitchTo(store.Load(name)); }
        catch (Exception e) when (e is IOException or InvalidDataException) { SwitchTo(new LaunchProfile()); Toast(e.Message); }
    }

    private void SwitchTo(LaunchProfile p)
    {
        profile = p;
        selectedProfile = ProfileNames.Contains(p.Name) ? p.Name : null;
        Raise(nameof(SelectedProfile));
        settings.LastProfile = p.Name;
        settings.Save();
        ProfileChanged();
    }

    private void DoSave()
    {
        var issues = ProfileValidator.Validate(profile, repo).Where(i => i.Blocking && i.Field != nameof(LaunchProfile.GameExe)).ToList();
        if (issues.Count > 0) { Toast("Not saved: " + string.Join(" ", issues.Select(i => i.Message))); return; }
        profile.Name = profile.Name.Trim();
        var existed = store.Exists(profile.Name);
        if (existed && selectedProfile != profile.Name &&
            MessageBox.Show($"A profile named '{profile.Name}' exists. Replace it? (The old one is kept as a .bak file.)", "Save profile",
                MessageBoxButton.YesNo, MessageBoxImage.Question) != MessageBoxResult.Yes) return;
        try
        {
            var backup = store.Save(profile);
            if (!ProfileNames.Contains(profile.Name)) ProfileNames.Add(profile.Name);
            SwitchTo(profile);
            Toast(backup != null ? $"Saved. The previous version is in {Path.GetFileName(backup)}." : "Saved.");
        }
        catch (Exception e) when (e is IOException or ArgumentException or InvalidDataException or UnauthorizedAccessException) { Toast($"Not saved: {e.Message}"); }
    }

    private void DoDelete()
    {
        if (MessageBox.Show($"Delete the profile '{profile.Name}'? It is moved to the 'deleted' folder, not erased.", "Delete profile",
                MessageBoxButton.YesNo, MessageBoxImage.Question) != MessageBoxResult.Yes) return;
        try
        {
            store.Delete(profile.Name);
            settings.LastProfile = null;
            LoadProfiles();
            Toast("Deleted (moved to the profiles\\deleted folder).");
        }
        catch (IOException e) { Toast($"Not deleted: {e.Message}"); }
    }

    private void DoImport()
    {
        var dlg = new OpenFileDialog { Filter = "Launcher profile (*.json)|*.json", Title = "Import a profile" };
        if (dlg.ShowDialog() != true) return;
        try
        {
            var p = ProfileStore.Read(dlg.FileName);
            p.Name = store.FreeName(p.Name);
            store.Save(p);
            ProfileNames.Add(p.Name);
            SwitchTo(p);
            Toast($"Imported as '{p.Name}'.");
        }
        catch (Exception e) when (e is IOException or InvalidDataException or ArgumentException) { Toast($"Not imported: {e.Message}"); }
    }

    private void DoExport()
    {
        var dlg = new SaveFileDialog { Filter = "Launcher profile (*.json)|*.json", FileName = profile.Name + ".json", Title = "Export the profile" };
        if (dlg.ShowDialog() != true) return;
        try { ProfileStore.Export(profile, dlg.FileName); Toast("Exported."); }
        catch (Exception e) when (e is IOException or InvalidDataException or UnauthorizedAccessException) { Toast($"Not exported: {e.Message}"); }
    }

    private void DoBrowseGame()
    {
        var dlg = new OpenFileDialog { Filter = "The game's executable|game_scarlett_master.exe;*.exe", Title = "Choose your copy of game_scarlett_master.exe" };
        if (!string.IsNullOrWhiteSpace(profile.GameExe) && Directory.Exists(Path.GetDirectoryName(profile.GameExe)))
            dlg.InitialDirectory = Path.GetDirectoryName(profile.GameExe);
        if (dlg.ShowDialog() != true) return;
        profile.GameExe = dlg.FileName;
        ProfileChanged();
        _ = RunChecksAsync();
    }

    private void DoBrowseRepo()
    {
        var dlg = new OpenFolderDialog { Title = "Choose the runkis-nvidia folder" };
        if (dlg.ShowDialog() != true) return;
        var found = RepoLayout.Find(dlg.FolderName);
        if (found == null) { Toast("That folder has no .victory-root and scripts\\play-nvidia.ps1."); return; }
        repo = found;
        settings.RepoRoot = found.Root;
        settings.Save();
        Raise(nameof(RepoText));
        Raise(nameof(RepoFound));
        Raise(nameof(CanStart));
        _ = RunChecksAsync();
    }

    private void PickFolder(Action<string> set)
    {
        var dlg = new OpenFolderDialog { Title = "Choose a folder outside the source tree" };
        if (dlg.ShowDialog() != true) return;
        set(dlg.FolderName);
        ProfileChanged();
    }

    // ---------------------------------------------------------------- running
    private async Task StartAsync(RunMode mode)
    {
        if (repo == null || !CanStart) return;
        Busy = true;
        SetState("Preparing", "Reading the tuned settings from play-nvidia.ps1", "Accent");
        try
        {
            var plan = await new PlanBuilder(repo).BuildAsync(EffectiveProfile.Clone(), mode);
            if (!File.Exists(plan.Victory.Replace("victory_sm.exe", "victory.exe")))
                throw new PlanException($"{plan.Victory} is not built. Build it with: {RepoLayout.BuildCommand(profile.Build)}");
            session?.Dispose();
            frames.Clear();
            FrameSamples = [];
            Problems.Clear();
            LogLines.Clear();
            cpuPid = null;
            frameTail = plan.FrameLogPath != null ? new FileTail(plan.FrameLogPath) : null;
            var s = new EmulatorSession(plan);
            s.Line += l => pending.Enqueue(l);
            s.ProblemFound += p => Application.Current.Dispatcher.BeginInvoke(() => Problems.Add(p));
            s.StateChanged += st => Application.Current.Dispatcher.BeginInvoke(() => OnSessionState(s, st));
            session = s;
            RunTitle = mode switch { RunMode.Warm => "Warm-up: building the pipeline cache", RunMode.Precompile => "Warm-up: translating shaders", _ => $"{GameName} · {SelectedStart?.Label}" };
            warm = mode switch { RunMode.Warm => new WarmProgress(), RunMode.Precompile => new WarmProgress(settings.LastShaderTotal ?? 0), _ => null };
            WarmPercent = warm?.Percent;
            s.Start();
            if (mode == RunMode.Play) Page = "Play";
        }
        catch (PlanException e) { SetState("Not started", e.Message, "Bad"); }
        catch (Exception e) when (e is IOException or InvalidOperationException or System.ComponentModel.Win32Exception)
        {
            SetState("Not started", e.Message, "Bad");
        }
        finally { Busy = false; CommandManager.InvalidateRequerySuggested(); }
    }

    private void OnSessionState(EmulatorSession s, SessionState st)
    {
        if (s != session) return;
        switch (st)
        {
            case SessionState.Running: SetState("Running", s.Plan.Mode == RunMode.Play ? "The first start translates shaders: slow for a few minutes." : s.Plan.CommandLine, "Good"); break;
            case SessionState.Stopping: SetState("Stopping", "Asking the game to close…", "Warn"); break;
            case SessionState.Failed: SetState("Could not start", s.ExitText ?? "", "Bad"); break;
            case SessionState.Exited:
                var ok = s.ExitCode == 0;
                SetState(ok ? "Finished" : s.StoppedByUser ? "Stopped" : "Ended with an error", s.ExitText ?? "", ok || s.StoppedByUser ? "Muted" : "Bad");
                if (!ok && !s.StoppedByUser && Problems.Count == 0)
                    Problems.Add(new Problem("exit", Severity.Error, "The run ended unexpectedly", s.ExitText ?? "", "See the log for the last lines before it ended.", ""));
                foreach (var t in Tiles) t.Unknown("Not running");
                WarmPercent = null;
                // Warm up, second step: the pipelines earlier plays recorded, once the shaders are in the cache.
                if (s.Plan.Mode == RunMode.Precompile && warmUpNext)
                {
                    warmUpNext = false;
                    if (ok)
                        Application.Current.Dispatcher.BeginInvoke(() => _ = StartAsync(RunMode.Warm));
                    else
                        Toast("Warm-up stopped: translating the shaders failed (see Logs).");
                }
                else if (s.Plan.Mode == RunMode.Warm && ok)
                    Toast("Warm-up done: shaders and recorded pipelines are cached. Pipelines get recorded while you play, so warm up again after playing new places.");
                break;
        }
        Raise(nameof(IsRunning));
        Raise(nameof(CanStart));
        CommandManager.InvalidateRequerySuggested();
    }

    private async Task StopAsync()
    {
        if (session == null) return;
        await session.StopAsync(TimeSpan.FromSeconds(8));
    }

    private void SetState(string s, string detail, string tone)
    {
        State = s;
        StateDetail = detail;
        StateTone = (Brush)Application.Current.Resources[tone];
    }

    // ---------------------------------------------------------------- monitoring
    public Tile Fps { get; } = new("FPS");
    public Tile FrameTime { get; } = new("Frame time");
    public Tile Low { get; } = new("1 % low");
    public Tile Hitches { get; } = new("Hitches ≥100 ms");
    public Tile CpuTile { get; } = new("CPU (emulator)");
    public Tile Ram { get; } = new("RAM (emulator)");
    public Tile Gpu { get; } = new("GPU 3D (emulator)");
    public Tile Vram { get; } = new("VRAM (emulator)");
    public Tile[] Tiles => [Fps, FrameTime, Low, Hitches, CpuTile, Ram, Gpu, Vram];

    private double[] frameSamples = [];
    public double[] FrameSamples { get => frameSamples; private set => Set(ref frameSamples, value); }

    private string systemLive = "";
    public string SystemLive { get => systemLive; private set => Set(ref systemLive, value); }
    public ObservableCollection<GpuRow> Gpus { get; }

    private void FastTick()
    {
        // Log lines in batches: one UI update per tick, not one per line.
        var added = 0;
        var moved = false;
        while (pending.TryDequeue(out var l) && added < 2000)
        {
            LogLines.Add(l);
            added++;
            moved |= warm?.Take(l.Text) == true;
        }
        if (moved && warm != null && session != null)
        {
            var precompile = session.Plan.Mode == RunMode.Precompile;
            RunTitle = (precompile ? "Translating shaders: " : "Building pipelines: ") + warm.Describe(precompile ? "shaders" : "pipelines");
            WarmPercent = warm.Percent;
            if (precompile && warm.FinishedTotal is long total && total != settings.LastShaderTotal)
            {
                settings.LastShaderTotal = total;
                settings.Save();
            }
        }
        while (LogLines.Count > 20000) LogLines.RemoveAt(0);
        if (added > 0 && AutoScroll) LogAdded?.Invoke();
        session?.Flush();

        if (frameTail != null && IsRunning)
        {
            try
            {
                var any = false;
                foreach (var line in frameTail.ReadNew())
                    if (FrameStats.TryParse(line, out var f)) { frames.Add(f); any = true; }
                if (any) FrameSamples = frames.Recent(400).Select(f => f.Ms).ToArray();
            }
            catch (IOException) { }
        }
    }

    public event Action? LogAdded;

    private async Task SlowTick()
    {
        if (sampling) return;
        sampling = true;
        try
        {
            var pid = session?.Pid;
            var chosen = Dxgi.Resolve(adapters, profile.Adapter).Adapter;
            var luid = chosen?.CounterLuid;
            var r = await Task.Run(() =>
            {
                var gpu = gpuCounters.Sample(pid, luid);
                var nv = nvml.Read();
                var sysCpu = cpu.Sample();
                var mem = SystemInfo.MemoryNow();
                Caption.Reading? caption = null;
                (long Private, long Working, TimeSpan Cpu)? proc = null;
                if (pid is int id)
                {
                    caption = Caption.Read(id);
                    try { using var p = Process.GetProcessById(id); proc = (p.PrivateMemorySize64, p.WorkingSet64, p.TotalProcessorTime); }
                    catch (Exception e) when (e is ArgumentException or InvalidOperationException or System.ComponentModel.Win32Exception) { }
                }
                return (gpu, nv, sysCpu, mem, caption, proc);
            });
            UpdateTiles(pid, r.gpu, r.caption, r.proc);
            UpdateSystem(r.gpu, r.nv, r.sysCpu, r.mem);
            // Fields edited through bindings change the profile object directly; its summaries follow here.
            Raise(nameof(ProfileSummary));
            Raise(nameof(ValidationText));
            Raise(nameof(GameName));
            Raise(nameof(GameFolder));
        }
        finally { sampling = false; }
    }

    private void UpdateTiles(int? pid, GpuUse gpu, Caption.Reading? caption, (long Private, long Working, TimeSpan Cpu)? proc)
    {
        if (pid == null || !IsRunning) return;
        var good = (Brush)Application.Current.Resources["Good"];
        var warn = (Brush)Application.Current.Resources["Warn"];
        var bad = (Brush)Application.Current.Resources["Bad"];
        var sum = frames.Summary(10);

        // FPS: the frame log when it is there (exact, every frame), else the layer's caption (once a second).
        if (sum.Fps is double fps)
            Fps.Show($"{fps:0.0}", "last 10 s, from the frame log", fps >= 25 ? good : fps >= 15 ? warn : bad);
        else if (caption != null)
            Fps.Show($"{caption.Fps}", "from the game window", caption.Fps >= 25 ? good : caption.Fps >= 15 ? warn : bad);
        else Fps.Unknown("No frames yet: the game has not presented a picture (shaders are still being translated).");

        if (sum.AvgMs is double ms) FrameTime.Show($"{ms:0.0} ms", sum.WorstMs is double w ? $"worst {w:0} ms" : "");
        else if (caption != null) FrameTime.Show($"{caption.Ms:0.0} ms", $"{caption.Draws} draws/frame");
        else FrameTime.Unknown("No frames yet.");

        if (sum.Low1Fps is double low && sum.Frames >= 100) Low.Show($"{low:0.0}", "fps of the slowest 1 % of frames");
        else Low.Unknown("Needs at least 100 frames in the last 10 s.");
        if (sum.Frames > 0) Hitches.Show($"{sum.Hitches}", "in the last 10 s", sum.Hitches == 0 ? good : sum.Hitches < 5 ? warn : bad);
        else Hitches.Unknown("No frames yet.");

        if (proc is { } p)
        {
            var now = DateTime.UtcNow;
            if (cpuPid == pid && lastCpuAt != default)
            {
                var pct = (p.Cpu - lastCpuTime).TotalMilliseconds / ((now - lastCpuAt).TotalMilliseconds * Environment.ProcessorCount) * 100;
                CpuTile.Show($"{Math.Clamp(pct, 0, 100):0}%", $"of all {Environment.ProcessorCount} threads");
            }
            else CpuTile.Unknown("Measured from the second sample on.");
            (cpuPid, lastCpuTime, lastCpuAt) = (pid, p.Cpu, now);
            Ram.Show($"{p.Private / MB / 1024:0.0} GB", $"private · {p.Working / MB / 1024:0.0} GB in RAM");
        }
        else { CpuTile.Unknown("The process could not be read."); Ram.Unknown("The process could not be read."); }

        if (gpu.Process3D is double g)
            Gpu.Show($"{g:0}%", gpu.ProcessCompute is double c && c > 0.5 ? $"compute {c:0}% · card {gpu.AdapterBusy:0}%" : $"card {gpu.AdapterBusy:0}%");
        else Gpu.Unknown(gpuCounters.Error ?? "Windows' GPU counters are measured from the second sample on.");
        if (gpu.ProcessDedicated is ulong v)
            Vram.Show($"{v / MB / 1024:0.00} GB", gpu.AdapterDedicated is ulong all && chosenVram > 0 ? $"card {all / MB / 1024:0.0} of {chosenVram / MB / 1024:0.0} GB" : "dedicated");
        else Vram.Unknown(gpuCounters.Error ?? "Windows reports no GPU memory for this process yet.");
    }

    private ulong chosenVram => Dxgi.Resolve(adapters, profile.Adapter).Adapter?.DedicatedVideoMemory ?? 0;

    private void UpdateSystem(GpuUse gpu, List<NvidiaReading> nv, double? sysCpu, (ulong Total, ulong Avail) mem)
    {
        SystemLive = $"CPU {(sysCpu is double c ? $"{c:0}%" : "—")}   ·   RAM {(mem.Total - mem.Avail) / MB / 1024:0.0} of {mem.Total / MB / 1024:0.0} GB in use";
        foreach (var row in Gpus)
        {
            var reading = row.Adapter.IsNvidia ? nv.FirstOrDefault(n => string.Equals(n.Name, row.Adapter.Name, StringComparison.OrdinalIgnoreCase)) : null;
            if (reading != null)
                row.Live = $"Load {Fmt(reading.GpuUtil, "%")} · VRAM {(reading.MemUsed is ulong u ? $"{u / MB / 1024:0.0}" : "—")} / {(reading.MemTotal is ulong t ? $"{t / MB / 1024:0.0}" : "—")} GB · "
                           + $"{Fmt(reading.TemperatureC, " °C")} · {Fmt(reading.GraphicsClockMhz, " MHz")} · "
                           + $"{(reading.PowerW is double w ? $"{w:0} W" : "—")}{(reading.PowerLimitW is double l ? $" of {l:0} W" : "")}";
            else if (row.Adapter.Software) row.Live = "Software adapter (not used for play).";
            else row.Live = row.Adapter.IsNvidia ? $"Live figures unavailable: {nvml.Error ?? "no NVML reading"}" : "Live figures come from NVIDIA's driver library; not available for this card.";
        }
    }

    private static string Fmt(uint? v, string unit) => v is uint x ? $"{x}{unit}" : "—";

    // ---------------------------------------------------------------- problems, logs, diagnostics
    public ObservableCollection<Problem> Problems { get; } = [];
    public ObservableCollection<LogLine> LogLines { get; } = [];
    public ICollectionView LogView { get; }
    private string logSearch = "";
    public string LogSearch { get => logSearch; set { if (Set(ref logSearch, value)) LogView.Refresh(); } }
    private bool errorsOnly;
    public bool ErrorsOnly { get => errorsOnly; set { if (Set(ref errorsOnly, value)) LogView.Refresh(); } }
    private bool autoScroll = true;
    public bool AutoScroll { get => autoScroll; set => Set(ref autoScroll, value); }

    public ObservableCollection<CheckResult> Checks { get; } = [];
    private bool checksRunning;
    public bool ChecksRunning { get => checksRunning; private set => Set(ref checksRunning, value); }
    private string checksSummary = "";
    public string ChecksSummary { get => checksSummary; private set => Set(ref checksSummary, value); }

    private async Task RunChecksAsync()
    {
        if (ChecksRunning) return;
        ChecksRunning = true;
        ChecksSummary = "Checking…";
        try
        {
            var p = EffectiveProfile.Clone();
            var results = await Task.Run(() => new DiagnosticRunner(repo).RunAsync(p));
            Checks.Clear();
            foreach (var c in results.OrderByDescending(c => c.Status)) Checks.Add(c);
            int fails = results.Count(c => c.Status == CheckStatus.Fail), warns = results.Count(c => c.Status == CheckStatus.Warning);
            ChecksSummary = fails > 0 ? $"{fails} problem(s) will stop a run, {warns} warning(s)" : warns > 0 ? $"Ready, with {warns} warning(s)" : "Everything checked is ready";
            Machine = SystemInfo.Read();
            Raise(nameof(Machine));
        }
        finally { ChecksRunning = false; CommandManager.InvalidateRequerySuggested(); }
    }

    private async Task PreviewAsync()
    {
        if (repo == null) { PreviewText = "No project folder."; return; }
        PreviewText = "Asking play-nvidia.ps1…";
        try
        {
            var p = profile.Clone();
            if (string.IsNullOrWhiteSpace(p.GameExe)) p.GameExe = @"C:\your\game\game_scarlett_master.exe";
            var b = new PlanBuilder(repo);
            var preset = await b.ResolveAsync(p, RunMode.Play);
            var plan = b.Compose(p, RunMode.Play, preset, DateTime.Now);
            var sb = new StringBuilder();
            sb.AppendLine(plan.CommandLine).AppendLine();
            foreach (var (k, v) in plan.Settings.OrderBy(kv => kv.Key, StringComparer.Ordinal))
                sb.Append(plan.Overridden.Contains(k) ? "* " : "  ").Append(k).Append('=').AppendLine(v.Length > 90 ? v[..90] + $"… ({v.Length} chars)" : v);
            sb.AppendLine().Append("* set by this profile over the script's value");
            PreviewText = sb.ToString();
        }
        catch (PlanException e) { PreviewText = e.Message; }
    }

    private string toast = "";
    public string ToastText { get => toast; private set => Set(ref toast, value); }
    private void Toast(string text)
    {
        ToastText = text;
        var t = new DispatcherTimer { Interval = TimeSpan.FromSeconds(6) };
        t.Tick += (_, _) => { if (ToastText == text) ToastText = ""; t.Stop(); };
        t.Start();
    }

    internal static void OpenFolder(string path)
    {
        Directory.CreateDirectory(path);
        Process.Start(new ProcessStartInfo("explorer.exe", $"\"{path}\"") { UseShellExecute = true })?.Dispose();
    }

    internal static void OpenFile(string? path)
    {
        if (path != null && File.Exists(path)) Process.Start(new ProcessStartInfo(path) { UseShellExecute = true })?.Dispose();
    }

    // ---------------------------------------------------------------- commands
    public Command Launch { get; }
    public Command Warm { get; }
    public Command Precompile { get; }
    public Command Stop { get; }
    public Command BrowseGame { get; }
    public Command BrowseRepo { get; }
    public Command BrowseCache { get; }
    public Command BrowseData { get; }
    public Command ResetFolders { get; }
    public Command NewProfile { get; }
    public Command Duplicate { get; }
    public Command Save { get; }
    public Command Delete { get; }
    public Command Import { get; }
    public Command Export { get; }
    public Command RunChecks { get; }
    public Command Preview { get; }
    public Command OpenLogs { get; }
    public Command OpenSessionLog { get; }
    public Command OpenCrashLog { get; }
    public Command CopyLog { get; }
    public Command ClearLog { get; }
    public Command Go { get; }
    public Command SavePreset { get; }
    /// <summary>Translates every shader into the cache, then builds every recorded pipeline into the driver's cache.</summary>
    public Command WarmUp { get; }
    private bool warmUpNext;

    // ---------------------------------------------------------------- for the Benchmark page
    internal RepoLayout? Repo => repo;
    internal ProfileStore Store => store;
    /// <summary>A play, warm-up or plan in progress: a benchmark may not start beside it.</summary>
    internal bool EmulatorBusy => Busy || IsRunning;
    internal void Notify(string text) => Toast(text);
    internal void BenchChanged() { Raise(nameof(CanStart)); CommandManager.InvalidateRequerySuggested(); }
    internal void ShowState(string s, string detail, string tone, string title) { SetState(s, detail, tone); RunTitle = title; }
    internal string MachineText => $"{Machine.CpuName}, {Machine.LogicalProcessors} threads, {Machine.TotalRam / MB / 1024:0} GB RAM, "
                                   + (Dxgi.Resolve(adapters, profile.Adapter).Adapter?.Name ?? "no card");

    /// <summary>True when the window may close; asks what to do with a run still going.</summary>
    public bool ConfirmClose()
    {
        if (Bench.Running)
        {
            if (MessageBox.Show("A benchmark is running.\n\nStop it and close? The current run is stopped the normal way first.", "Runkis Launcher",
                    MessageBoxButton.YesNo, MessageBoxImage.Question) != MessageBoxResult.Yes) return false;
            Bench.CancelAndWait(TimeSpan.FromSeconds(25));
            return true;
        }
        if (!IsRunning) return true;
        var answer = MessageBox.Show("The game is still running.\n\nYes: stop it and close.\nNo: close the launcher and leave the game running.\nCancel: stay.",
            "Runkis Launcher", MessageBoxButton.YesNoCancel, MessageBoxImage.Question);
        if (answer == MessageBoxResult.Cancel) return false;
        if (answer == MessageBoxResult.Yes) session?.StopAsync(TimeSpan.FromSeconds(8)).Wait(TimeSpan.FromSeconds(10));
        return true;
    }

    public void Dispose()
    {
        fast.Stop();
        slow.Stop();
        session?.Dispose();
        nvml.Dispose();
    }
}
