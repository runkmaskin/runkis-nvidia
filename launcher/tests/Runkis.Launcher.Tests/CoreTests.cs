using Runkis.Launcher.Core;
using Xunit;

namespace Runkis.Launcher.Tests;

public sealed class TempDir : IDisposable
{
    public string Path { get; } = System.IO.Path.Combine(System.IO.Path.GetTempPath(), "runkis-tests-" + Guid.NewGuid().ToString("N"));
    public TempDir() => Directory.CreateDirectory(Path);
    public void Dispose() { try { Directory.Delete(Path, true); } catch (IOException) { } }
}

public class ProfileTests
{
    private static LaunchProfile Valid(TempDir d)
    {
        var exe = Path.Combine(d.Path, "game_scarlett_master.exe");
        File.WriteAllText(exe, "");
        return new LaunchProfile { Name = "t", GameExe = exe };
    }

    [Fact]
    public void Default_profile_with_a_game_is_valid()
    {
        using var d = new TempDir();
        Assert.DoesNotContain(ProfileValidator.Validate(Valid(d)), i => i.Blocking);
    }

    [Theory]
    [InlineData(2160, ConsoleModel.SeriesS, false)]
    [InlineData(1080, ConsoleModel.SeriesS, true)]
    [InlineData(2160, ConsoleModel.SeriesX, true)]
    [InlineData(1000, ConsoleModel.SeriesS, false)]   // not a height the script takes
    [InlineData(900, ConsoleModel.SeriesS, true)]
    public void Resolution_must_fit_the_console(int height, ConsoleModel console, bool ok)
    {
        using var d = new TempDir();
        var p = Valid(d);
        p.Resolution = height;
        p.Console = console;
        Assert.Equal(ok, !ProfileValidator.Validate(p).Any(i => i.Field == nameof(LaunchProfile.Resolution)));
    }

    [Fact]
    public void Ranges_follow_what_the_layer_accepts()
    {
        using var d = new TempDir();
        var p = Valid(d);
        p.MipDrop = 4; p.WideThreads = 0; p.TargetRoomMb = 9000; p.Steady = 1.5;
        var fields = ProfileValidator.Validate(p).Select(i => i.Field).ToHashSet();
        Assert.Contains(nameof(LaunchProfile.MipDrop), fields);
        Assert.Contains(nameof(LaunchProfile.WideThreads), fields);
        Assert.Contains(nameof(LaunchProfile.TargetRoomMb), fields);
        Assert.Contains(nameof(LaunchProfile.Steady), fields);
    }

    [Fact]
    public void Folders_inside_the_source_tree_are_refused()
    {
        using var d = new TempDir();
        File.WriteAllText(Path.Combine(d.Path, RepoLayout.RootMarker), "");
        var p = Valid(d);
        p.CacheDir = Path.Combine(d.Path, "cache");
        var issues = ProfileValidator.Validate(p, new RepoLayout(d.Path));
        Assert.Contains(issues, i => i.Field == nameof(LaunchProfile.CacheDir) && i.Blocking);
    }

    [Fact]
    public void Store_saves_backs_up_and_deletes_to_a_bin()
    {
        using var d = new TempDir();
        var store = new ProfileStore(d.Path);
        var p = Valid(d);
        p.Name = "Mine";
        Assert.Null(store.Save(p));
        p.Air = 1.1;
        var backup = store.Save(p);
        Assert.NotNull(backup);
        Assert.Equal(0.3, ProfileStore.Read(backup!).Air);
        Assert.Equal(1.1, store.Load("Mine").Air);
        Assert.Equal("Mine 2", store.FreeName("Mine"));
        var bin = store.Delete("Mine");
        Assert.True(File.Exists(bin));
        Assert.Empty(store.List());
    }

    [Fact]
    public void A_broken_profile_file_is_refused_with_a_reason()
    {
        using var d = new TempDir();
        var f = Path.Combine(d.Path, "bad.json");
        File.WriteAllText(f, "{ not json");
        var e = Assert.Throws<InvalidDataException>(() => ProfileStore.Read(f));
        Assert.Contains("bad.json", e.Message);
    }

    [Fact]
    public void Enums_are_stored_by_name()
    {
        using var d = new TempDir();
        var store = new ProfileStore(d.Path);
        var p = Valid(d);
        p.Console = ConsoleModel.SeriesX;
        store.Save(p);
        Assert.Contains("\"SeriesX\"", File.ReadAllText(store.PathOf(p.Name)));
    }
}

public class PresetTests
{
    [Fact]
    public void Presets_keep_the_users_game_folders_and_card()
    {
        var mine = new LaunchProfile { GameExe = @"D:\g\game_scarlett_master.exe", CacheDir = @"D:\c", Adapter = "#1", Build = BuildKind.Release, Quality = true, Air = 1.2 };
        foreach (var preset in Presets.All)
        {
            var p = Presets.Make(preset, mine);
            Assert.Equal(mine.GameExe, p.GameExe);
            Assert.Equal(mine.CacheDir, p.CacheDir);
            Assert.Equal(mine.Adapter, p.Adapter);
            Assert.Equal(0.3, p.Air);   // the rest starts from the script's set, not the profile's choices
            Assert.Equal(preset.Name, p.Name);
        }
    }

    [Fact]
    public void Every_preset_is_a_valid_profile()
    {
        using var d = new TempDir();
        var exe = Path.Combine(d.Path, "game_scarlett_master.exe");
        File.WriteAllText(exe, "");
        foreach (var preset in Presets.All)
            Assert.DoesNotContain(ProfileValidator.Validate(Presets.Make(preset, new LaunchProfile { GameExe = exe })), i => i.Blocking);
        Assert.Equal(Presets.All.Count, Presets.All.Select(p => p.Id).Distinct().Count());
    }

    [Fact]
    public void Presets_set_what_they_say()
    {
        var basis = new LaunchProfile { GameExe = "x.exe" };
        var fps = Presets.Make(Presets.ById("fps")!, basis);
        Assert.Equal(480, fps.Resolution);
        Assert.True(fps.SkipHairSim && fps.LeaveOut);
        var x = Presets.Make(Presets.ById("seriesx")!, basis);
        Assert.Equal(ConsoleModel.SeriesX, x.Console);
        Assert.Null(x.Resolution);
        Assert.Equal(["-Exe", "x.exe", "-Build", "diag", "-SeriesX", "-NoSmoothMotion", "-PrintEnv"], PlanBuilder.ScriptArgs(x, RunMode.Play));
        Assert.Equal(DredMode.Light, Presets.Make(Presets.ById("safe")!, basis).Dred);
    }
}

public class ThisPcPresetTests
{
    private const ulong GB = 1UL << 30;
    private static GpuAdapter Card(int i, string name, double gb, uint vendor = 0x10DE) =>
        new(i, name, vendor, 1, (ulong)(gb * GB), 0, i, false, "32.0.16.2114");
    private static SystemSnapshot Pc(int threads, double ramGb) =>
        new("cpu", threads / 2, threads, (ulong)(ramGb * GB), 0, (ulong)(ramGb * 2 * GB), 0, "win", true, "Balanced", false);

    [Fact]
    public void A_bigger_pc_keeps_the_measured_set()
    {
        // Nothing measured says a 12 GB card or 16 threads want other values: the measured ones stay (a 3 GB
        // render-target reserve and full-size textures had made frames worse).
        var preset = Presets.ForThisPc([Card(0, "NVIDIA GeForce RTX 4070 SUPER", 12)], Pc(16, 32), null);
        var p = Presets.Make(preset, new LaunchProfile { GameExe = "x.exe" });
        Assert.Null(p.MipDrop);
        Assert.Null(p.TargetRoomMb);
        Assert.Null(p.WideThreads);
        Assert.Equal(540, p.Resolution);
        Assert.Contains("12 GB", preset.Description);
    }

    [Fact]
    public void The_measured_hardware_gets_the_measured_set()
    {
        var p = Presets.Make(Presets.ForThisPc([Card(0, "NVIDIA GeForce RTX 2070 SUPER", 8)], Pc(12, 32), null), new LaunchProfile());
        Assert.Null(p.MipDrop);
        Assert.Null(p.TargetRoomMb);
        Assert.Null(p.WideThreads);
    }

    [Fact]
    public void A_small_card_gets_480p_and_smaller_textures()
    {
        var p = Presets.Make(Presets.ForThisPc([Card(0, "NVIDIA GeForce RTX 3050", 6)], Pc(8, 16), null), new LaunchProfile());
        Assert.Equal(480, p.Resolution);
        Assert.Equal(1, p.MipDrop);
        Assert.Equal(1024, p.MipDropMinSize);
        Assert.Equal(4, p.WideThreads);   // 8 threads: half
    }

    [Fact]
    public void The_nvidia_card_is_picked_over_an_integrated_default()
    {
        var cards = new[] { Card(0, "AMD Radeon(TM) Graphics", 0.5, 0x1002), Card(1, "NVIDIA GeForce RTX 4070 SUPER", 12) };
        var preset = Presets.ForThisPc(cards, Pc(16, 32), null);
        Assert.Equal("#1", Presets.Make(preset, new LaunchProfile()).Adapter);
        Assert.Equal("#0", Presets.Make(preset, new LaunchProfile { Adapter = "#0" }).Adapter);   // a card the user chose stays
    }
}

public class PlanTests
{
    [Fact]
    public void Script_switches_follow_the_profile()
    {
        var p = new LaunchProfile { GameExe = @"D:\g\game_scarlett_master.exe", Console = ConsoleModel.SeriesX, Quality = true, Resolution = 720, Adapter = "#1", WarmLimit = 50 };
        var a = PlanBuilder.ScriptArgs(p, RunMode.Warm);
        Assert.Equal(["-Exe", @"D:\g\game_scarlett_master.exe", "-Build", "diag", "-SeriesX", "-Quality", "-Resolution", "720", "-Adapter", "#1",
            "-NoSmoothMotion", "-Warm", "-WarmLimit", "50", "-PrintEnv"], a);
    }

    [Fact]
    public void Script_variables_use_invariant_numbers()
    {
        var v = PlanBuilder.ScriptVariables(new LaunchProfile { Air = 0.25, Dred = DredMode.Light });
        Assert.Equal("0.25", v["VN_AIR"]);
        Assert.Equal("light", v["VN_DRED"]);
    }

    [Fact]
    public void Profile_overrides_go_over_the_script_and_paths_are_set()
    {
        var preset = new ScriptPreset
        {
            Victory = @"C:\r\target\diag\victory.exe", Workdir = @"C:\r", SmoothMotion = false,
            Args = ["run", @"D:\g\game_scarlett_master.exe", "--execute", "--console", "series-s"],
            Settings = new() { ["VICTORY_PIPELINE_WAIT_MS"] = "500", ["VICTORY_RELAY"] = "2" },
        };
        var p = new LaunchProfile { GameExe = @"D:\g\game_scarlett_master.exe", PipelineWaitMs = 250, Panel = true };
        var plan = new PlanBuilder(new RepoLayout(@"C:\r")).Compose(p, RunMode.Play, preset, new DateTime(2026, 11, 27, 9, 0, 0));
        Assert.Equal("250", plan.Settings["VICTORY_PIPELINE_WAIT_MS"]);
        Assert.Equal("2", plan.Settings["VICTORY_RELAY"]);
        Assert.Equal("1", plan.Settings["VICTORY_PANEL"]);
        Assert.Equal(@"D:\g", plan.Settings["VICTORY_GAME_DIR"]);
        Assert.Equal("1", plan.Settings["NO_COLOR"]);
        Assert.Contains("VICTORY_PIPELINE_WAIT_MS", plan.Overridden);
        Assert.DoesNotContain("VICTORY_RELAY", plan.Overridden);
        Assert.EndsWith("20261127-090000-play.frames.txt", plan.FrameLogPath);
        Assert.Equal(plan.FrameLogPath, plan.Settings["VICTORY_FRAME_LOG"]);
    }

    [Fact]
    public void Upscale_to_the_window_sets_its_switches_only_when_on()
    {
        var off = PlanBuilder.Overrides(new LaunchProfile());
        Assert.False(off.ContainsKey("VICTORY_PRESENT_SCALE"));
        Assert.False(off.ContainsKey("VICTORY_PRESENT_SHARPEN"));
        var on = PlanBuilder.Overrides(new LaunchProfile { PresentScale = true, PresentSharpen = 0.45 });
        Assert.Equal("1", on["VICTORY_PRESENT_SCALE"]);
        Assert.Equal("0.45", on["VICTORY_PRESENT_SHARPEN"]);   // invariant: a point, also on a Swedish PC
        Assert.Contains(ProfileValidator.Validate(new LaunchProfile { PresentSharpen = 1.5 }), i => i.Field == nameof(LaunchProfile.PresentSharpen));
    }

    [Fact]
    public void Display_defaults_set_no_variable()
    {
        var o = PlanBuilder.Overrides(new LaunchProfile());
        Assert.False(o.ContainsKey("VICTORY_WINDOW"));
        Assert.False(o.ContainsKey("VICTORY_WINDOW_SIZE"));
        Assert.False(o.ContainsKey("VICTORY_MONITOR"));
    }

    [Theory]
    [InlineData(WindowMode.Borderless, "", 0, "borderless", null, null)]
    [InlineData(WindowMode.Fullscreen, "1600x900", 2, "fullscreen", null, "2")]   // a size is for a framed window only
    [InlineData(WindowMode.Windowed, " 1600 X 900 ", 0, null, "1600x900", null)]
    [InlineData(WindowMode.Windowed, "", 1, null, null, "1")]
    public void Display_choices_map_to_the_layers_variables(WindowMode mode, string size, int monitor, string? window, string? windowSize, string? monitorVar)
    {
        var o = PlanBuilder.Overrides(new LaunchProfile { Window = mode, WindowSize = size, Monitor = monitor });
        Assert.Equal(window, o.GetValueOrDefault("VICTORY_WINDOW"));
        Assert.Equal(windowSize, o.GetValueOrDefault("VICTORY_WINDOW_SIZE"));
        Assert.Equal(monitorVar, o.GetValueOrDefault("VICTORY_MONITOR"));
    }

    [Fact]
    public void Display_choices_are_checked()
    {
        static List<ValidationIssue> Check(LaunchProfile p, int connected = 2) =>
            ProfileValidator.Validate(p, null, Enumerable.Range(1, connected).Select(n => new DisplayMonitor(n, 0, 0, 1920, 1080, 1920, 1040, n == 1)).ToList());
        Assert.Contains(Check(new LaunchProfile { WindowSize = "big" }), i => i.Field == nameof(LaunchProfile.WindowSize) && i.Blocking);
        Assert.Contains(Check(new LaunchProfile { WindowSize = "0x900" }), i => i.Field == nameof(LaunchProfile.WindowSize) && i.Blocking);
        Assert.Contains(Check(new LaunchProfile { Window = WindowMode.Borderless, WindowSize = "1600x900" }),
            i => i.Field == nameof(LaunchProfile.WindowSize) && !i.Blocking);
        Assert.DoesNotContain(Check(new LaunchProfile { WindowSize = "1600x900", Monitor = 2 }),
            i => i.Field is nameof(LaunchProfile.WindowSize) or nameof(LaunchProfile.Monitor));
        Assert.Contains(Check(new LaunchProfile { Monitor = 3 }), i => i.Field == nameof(LaunchProfile.Monitor) && !i.Blocking);
        Assert.Contains(Check(new LaunchProfile { Monitor = -1 }), i => i.Field == nameof(LaunchProfile.Monitor) && i.Blocking);
        Assert.Contains(Check(new LaunchProfile { Window = (WindowMode)7 }), i => i.Field == nameof(LaunchProfile.Window));
    }

    [Fact]
    public void Display_parsing_matches_the_layer()
    {
        Assert.True(Displays.TryParseSize("1280x720", out var w, out var h));
        Assert.Equal((1280, 720), (w, h));
        Assert.True(Displays.TryParseSize(" 1280 X 720 ", out _, out _));
        foreach (var bad in new[] { "", "1280", "0x720", "-1x720", "1x2x3", "99999x720", "+5x5", "a x b" })
            Assert.False(Displays.TryParseSize(bad, out _, out _), bad);
    }

    private static readonly List<DisplayMonitor> TwoMonitors =
    [
        new(1, -1920, 0, 1920, 1080, 1920, 1040, false),
        new(2, 0, 0, 2560, 1440, 2560, 1392, true),
    ];

    [Fact]
    public void A_monitor_is_picked_as_the_layer_picks_it()
    {
        Assert.Equal(1, Displays.Pick(TwoMonitors, 1)!.Number);
        Assert.Equal(2, Displays.Pick(TwoMonitors, 0)!.Number);   // the main monitor
        Assert.Equal(2, Displays.Pick(TwoMonitors, 5)!.Number);   // not connected: the main monitor
        Assert.Null(Displays.Pick([], 0));
    }

    [Fact]
    public void Window_sizes_are_the_common_ones_that_fit_then_the_monitors_own()
    {
        Assert.Equal(["1280x720", "1600x900", "1920x1080"], Displays.WindowSizes(TwoMonitors[1]).Take(3));
        Assert.Equal("2560x1440", Displays.WindowSizes(TwoMonitors[1]).Last());
        Assert.Equal(["1280x720", "1600x900", "1920x1080"], Displays.WindowSizes(TwoMonitors[0]));
        Assert.Equal(5, Displays.WindowSizes(null).Count);
    }

    [Fact]
    public void The_sharp_upscale_is_suggested_for_a_covered_monitor_above_1080p()
    {
        Assert.True(Displays.SuggestPresentScale(WindowMode.Borderless, TwoMonitors[1]));
        Assert.True(Displays.SuggestPresentScale(WindowMode.Fullscreen, TwoMonitors[1]));
        Assert.False(Displays.SuggestPresentScale(WindowMode.Windowed, TwoMonitors[1]));
        Assert.False(Displays.SuggestPresentScale(WindowMode.Fullscreen, TwoMonitors[0]));
        Assert.False(Displays.SuggestPresentScale(WindowMode.Fullscreen, null));
    }

    [Fact]
    public void Presets_keep_the_players_window()
    {
        var mine = new LaunchProfile { Window = WindowMode.Borderless, WindowSize = "1600x900", Monitor = 2 };
        Assert.All(Presets.All, x =>
        {
            var p = Presets.Make(x, mine);
            Assert.Equal((WindowMode.Borderless, "1600x900", 2), (p.Window, p.WindowSize, p.Monitor));
        });
    }

    [Fact]
    public void Display_choices_survive_the_profile_file()
    {
        using var d = new TempDir();
        var store = new ProfileStore(d.Path);
        store.Save(new LaunchProfile { Name = "screen", Window = WindowMode.Fullscreen, WindowSize = "1280x720", Monitor = 2 });
        Assert.Contains("\"Fullscreen\"", File.ReadAllText(store.PathOf("screen")));
        var back = store.Load("screen");
        Assert.Equal((WindowMode.Fullscreen, "1280x720", 2), (back.Window, back.WindowSize, back.Monitor));
    }

    [Fact]
    public void Upscale_to_the_window_follows_the_profile_not_the_preset()
    {
        var preset = Presets.ForThisPc([new GpuAdapter(0, "NVIDIA GeForce RTX 4070 SUPER", 0x10DE, 1, 12UL << 30, 0, 0, false, "32.0.16.2114")],
            new SystemSnapshot("cpu", 8, 16, 32UL << 30, 0, 64UL << 30, 0, "win", true, "Balanced", false), null);
        var p = Presets.Make(preset, new LaunchProfile { GameExe = "x.exe", PresentScale = true });
        Assert.True(p.PresentScale);    // kept from the profile, like the window mode
        Assert.All(Presets.All, x => Assert.False(Presets.Make(x, new LaunchProfile()).PresentScale));
    }

    [Fact]
    public void Smooth_motion_starts_the_renamed_copy()
    {
        var preset = new ScriptPreset { Victory = @"C:\r\target\diag\victory.exe", SmoothMotion = true, Args = ["warm"], Settings = new() { ["A"] = "1" } };
        var plan = new PlanBuilder(new RepoLayout(@"C:\r")).Compose(new LaunchProfile { GameExe = @"D:\g\x.exe" }, RunMode.Warm, preset, DateTime.Now);
        Assert.EndsWith(@"diag\victory_sm.exe", plan.Victory);
        Assert.Null(plan.FrameLogPath);
    }

    [Fact]
    public void Script_errors_come_through_as_plain_text()
    {
        var e = Assert.Throws<PlanException>(() => PlanBuilder.ParsePreset("Series S outputs 1080p: 2160p is above it.", ""));
        Assert.Contains("2160p is above it", e.Message);
    }

    /// <summary>The real script, run as the launcher runs it: the tuned settings come back and the switches act.</summary>
    [Fact]
    public async Task The_real_script_prints_its_settings()
    {
        var repo = RepoLayout.Find(AppContext.BaseDirectory);
        Assert.NotNull(repo);
        var b = new PlanBuilder(repo!);
        var speed = await b.ResolveAsync(new LaunchProfile { GameExe = @"D:\none\game_scarlett_master.exe" }, RunMode.Play);
        Assert.Equal("2", speed.Settings["VICTORY_RELAY"]);
        Assert.Equal("0.5", speed.Settings["VICTORY_RENDER_SCALE"]);
        Assert.Equal("0.3", speed.Settings["VICTORY_AMBIENT_AIR"]);   // VN_AIR from the profile reached the script
        Assert.Contains("--execute", speed.Args);
        Assert.False(speed.SmoothMotion);
        var quality = await b.ResolveAsync(new LaunchProfile { GameExe = @"D:\none\game_scarlett_master.exe", Quality = true, Resolution = 720 }, RunMode.Play);
        Assert.Equal("1", quality.Settings["VICTORY_RELAY"]);
        Assert.Equal("0.6667", quality.Settings["VICTORY_RENDER_SCALE"]);
        var warm = await b.ResolveAsync(new LaunchProfile { GameExe = @"D:\none\game_scarlett_master.exe", WarmLimit = 7 }, RunMode.Warm);
        Assert.Equal(["warm", "--limit", "7"], warm.Args);
    }
}

public class EndToEndTests
{
    /// <summary>
    /// The whole launch path against the real victory.exe and the real script, with a stand-in "title": a Windows
    /// console tool copied outside the source tree. What the layer makes of it does not matter here; that the run
    /// starts, its output lands in the log and the list, it ends, and the end is explained, does.
    /// </summary>
    [Fact]
    public async Task A_run_starts_logs_ends_and_is_explained()
    {
        var repo = RepoLayout.Find(AppContext.BaseDirectory)!;
        if (!File.Exists(repo.VictoryExe(BuildKind.Diag))) return;   // nothing built on this machine
        using var d = new TempDir();
        var game = Path.Combine(d.Path, "game_scarlett_master.exe");
        File.Copy(Path.Combine(Environment.SystemDirectory, "whoami.exe"), game);
        var p = new LaunchProfile { GameExe = game, DataDir = Path.Combine(d.Path, "data"), CacheDir = Path.Combine(d.Path, "cache") };
        var plan = await new PlanBuilder(repo).BuildAsync(p, RunMode.Play);

        using var s = new EmulatorSession(plan);
        var lines = new List<LogLine>();
        var ended = new TaskCompletionSource();
        s.Line += l => { lock (lines) lines.Add(l); };
        s.StateChanged += st => { if (st is SessionState.Exited or SessionState.Failed) ended.TrySetResult(); };
        s.Start();
        Assert.Equal(SessionState.Running, s.State);
        var done = await Task.WhenAny(ended.Task, Task.Delay(TimeSpan.FromSeconds(60)));
        if (done != ended.Task) { await s.StopAsync(TimeSpan.FromSeconds(2)); }
        await ended.Task.WaitAsync(TimeSpan.FromSeconds(15));

        Assert.Equal(SessionState.Exited, s.State);
        Assert.NotNull(s.ExitCode);
        Assert.False(string.IsNullOrEmpty(s.ExitText));
        lock (lines) Assert.NotEmpty(lines);
        s.Dispose();
        var log = await File.ReadAllTextAsync(plan.LogPath);
        Assert.Contains("[launcher] started process", log);
        Assert.Contains("VICTORY_RELAY=2", log);           // the script's tuned set reached the run
        Assert.Contains("[launcher] exited with", log);
        Assert.DoesNotContain((char)27 + "[", log, StringComparison.Ordinal);              // NO_COLOR: no terminal colour codes in the file
        File.Delete(plan.LogPath);
    }
}

public class StopTests
{
    /// <summary>A run with no window to close is ended after the grace time, and the end says the launcher did it.</summary>
    [Fact]
    public async Task Stop_ends_a_run_that_does_not_close_itself()
    {
        using var d = new TempDir();
        var plan = new LaunchPlan
        {
            Mode = RunMode.Play, Victory = Path.Combine(Environment.SystemDirectory, "PING.EXE"), WorkDir = d.Path,
            Args = ["-n", "60", "127.0.0.1"], Settings = new Dictionary<string, string>(), Overridden = [],
            LogPath = Path.Combine(d.Path, "s.log"), GameExe = "x", DataDir = d.Path, CacheDir = d.Path,
        };
        using var s = new EmulatorSession(plan);
        var ended = new TaskCompletionSource();
        s.StateChanged += st => { if (st == SessionState.Exited) ended.TrySetResult(); };
        s.Start();
        Assert.Equal(SessionState.Running, s.State);
        var started = DateTime.UtcNow;
        await s.StopAsync(TimeSpan.FromSeconds(1));
        await ended.Task.WaitAsync(TimeSpan.FromSeconds(10));
        Assert.True(DateTime.UtcNow - started < TimeSpan.FromSeconds(8));
        Assert.Equal("Stopped by the launcher.", s.ExitText);
    }
}

public class MonitorTests
{
    [Fact]
    public void Frame_log_lines_parse_and_others_do_not()
    {
        Assert.True(FrameStats.TryParse("12.345 55.20 6012", out var f));
        Assert.Equal(55.2, f.Ms, 3);
        Assert.Equal(6012, f.Draws);
        Assert.False(FrameStats.TryParse("garbage", out _));
        Assert.False(FrameStats.TryParse("1,5 2,5 3", out _));
        Assert.False(FrameStats.TryParse("12.345 55.20", out _));
    }

    [Fact]
    public void Frame_log_lines_with_the_wait_columns_parse_the_same()
    {
        // The three columns, then the waits the layer may add (docs/frame-pacing.md).
        Assert.True(FrameStats.TryParse("12.345 55.20 6012 31.40 0.00 0.62 0.05 0.00 0", out var f));
        Assert.Equal(12.345, f.At, 3);
        Assert.Equal(55.2, f.Ms, 3);
        Assert.Equal(6012, f.Draws);
        Assert.True(FrameStats.TryParse("12.345 55.20 6012", out var g));
        Assert.Equal(f, g);
        // The first three still have to be numbers.
        Assert.False(FrameStats.TryParse("12.345 x 6012 1.0", out _));
    }

    [Fact]
    public void Summary_counts_fps_lows_and_hitches()
    {
        var s = new FrameStats();
        double t = 0;
        for (var i = 0; i < 198; i++) { t += 0.05; s.Add(new(t, 50, 100)); }   // 20 fps
        t += 0.2; s.Add(new(t, 200, 100));                                     // two hitches
        t += 0.15; s.Add(new(t, 150, 100));
        var sum = s.Summary(60);
        Assert.Equal(200, sum.Frames);
        Assert.Equal(2, sum.Hitches);
        Assert.Equal(200, sum.WorstMs);
        Assert.Equal(200 * 1000.0 / (198 * 50 + 200 + 150), sum.Fps!.Value, 6);   // frames over their total time
        Assert.Equal(1000.0 / 175, sum.Low1Fps!.Value, 6);   // the slowest 1 % (2 of 200 frames): 200 and 150 ms
    }

    [Fact]
    public void Nothing_known_is_null_not_zero()
    {
        var sum = new FrameStats().Summary();
        Assert.Null(sum.Fps);
        Assert.Null(sum.Low1Fps);
    }

    [Fact]
    public void File_tail_returns_only_whole_new_lines()
    {
        using var d = new TempDir();
        var f = Path.Combine(d.Path, "frames.txt");
        using var w = new StreamWriter(new FileStream(f, FileMode.Create, FileAccess.Write, FileShare.ReadWrite)) { AutoFlush = true };
        var tail = new FileTail(f);
        w.Write("1.0 50 1\n2.0 5");
        Assert.Equal(["1.0 50 1"], tail.ReadNew());
        w.Write("0 2\n");
        Assert.Equal(["2.0 50 2"], tail.ReadNew());
        Assert.Empty(tail.ReadNew());
    }

    [Fact]
    public void Caption_reading_matches_the_layer_format()
    {
        // victory-gpu host.rs: format!(" | VIctory {fps:.0} fps {ms:.1} ms/frame {draws:.0} draws/frame")
        var r = Caption.Parse("Game | VIctory 18 fps 55.4 ms/frame 6012 draws/frame");
        Assert.NotNull(r);
        Assert.Equal(18, r!.Fps);
        Assert.Equal(55.4, r.Ms, 3);
        Assert.Null(Caption.Parse("Game"));
    }

    [Theory]
    [InlineData("2026-11-27T08:00:00Z ERROR victory_gpu::host: x; device removed: 0x887A0005", "device-removed")]
    [InlineData("VIctory: Windows commit limit reached while committing 65536 bytes of pool: x", "commit-limit")]
    [InlineData("WARN victory_gpu::host: VICTORY_ADAPTER names no hardware card; Windows' default card is used", "adapter-missing")]
    [InlineData("ERROR victory_loader::thunks: unimplemented import called dll=x symbol=y", "unimplemented")]
    [InlineData("this CPU does not support AVX2, which Xbox Series titles require", "avx2")]
    public void Known_messages_are_explained(string line, string id) => Assert.Equal(id, LogAnalyzer.Match(line)?.Id);

    [Fact]
    public void Ordinary_lines_are_not_problems()
    {
        Assert.Null(LogAnalyzer.Match("INFO victory_gpu::host: 18.2 fps, 55.0 ms/frame (40.1 preparing ...)"));
        Assert.Null(LogAnalyzer.Match("INFO title bytes compared with AVX2 on six threads"));
    }

    [Fact]
    public void Exit_codes_are_described()
    {
        Assert.Contains("normally", LogAnalyzer.DescribeExit(0));
        Assert.Contains("access violation", LogAnalyzer.DescribeExit(unchecked((int)0xC0000005)));
        Assert.Contains("panic", LogAnalyzer.DescribeExit(101));
    }
}

public class HardwareTests
{
    private static GpuAdapter A(int i, string name, bool sw = false) => new(i, name, 0x10DE, 1, 8UL << 30, 0, i, sw, "32.0.16.2114");

    [Fact]
    public void Adapter_choice_follows_the_layer_rule()
    {
        var list = new[] { A(0, "AMD Radeon(TM) Graphics"), A(1, "NVIDIA GeForce RTX 4070 SUPER"), A(2, "Microsoft Basic Render Driver", sw: true) };
        Assert.Equal(1, Dxgi.Resolve(list, "4070").Adapter!.Index);
        Assert.Equal(1, Dxgi.Resolve(list, "rtx").Adapter!.Index);
        Assert.Equal(1, Dxgi.Resolve(list, "#1").Adapter!.Index);
        Assert.Equal(0, Dxgi.Resolve(list, "").Adapter!.Index);
        var (fallback, matched) = Dxgi.Resolve(list, "#2");   // software adapters never match
        Assert.False(matched);
        Assert.Equal(0, fallback!.Index);
    }

    [Fact]
    public void Nvidia_version_comes_from_the_windows_one() => Assert.Equal("621.14", A(0, "x").NvidiaVersion);

    [Fact]
    public void This_pc_lists_at_least_one_adapter_with_a_driver_version()
    {
        var list = Dxgi.Adapters();
        Assert.NotEmpty(list);
        Assert.Contains(list, a => !a.Software && a.DriverVersion != null && a.DedicatedVideoMemory > 0);
    }

    [Fact]
    public void Memory_and_cores_are_read()
    {
        var s = SystemInfo.Read();
        Assert.True(s.TotalRam > 1UL << 30);
        Assert.True(s.PhysicalCores > 0 && s.PhysicalCores <= s.LogicalProcessors);
        Assert.True(s.CommitLimit >= s.TotalRam);
    }
}
