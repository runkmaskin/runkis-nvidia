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
        Assert.Empty(ProfileValidator.Validate(Valid(d)).Where(i => i.Blocking));
    }

    [Theory]
    [InlineData(2160, ConsoleModel.SeriesS, false)]
    [InlineData(1080, ConsoleModel.SeriesS, true)]
    [InlineData(2160, ConsoleModel.SeriesX, true)]
    [InlineData(900, ConsoleModel.SeriesS, false)]
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
