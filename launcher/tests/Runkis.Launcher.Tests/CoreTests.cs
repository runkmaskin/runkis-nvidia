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

    [Fact]
    public void Ranges_follow_what_the_layer_accepts()
    {
        using var d = new TempDir();
        var p = Valid(d);
        p.WideThreads = 0; p.TargetRoomMb = 9000; p.FrameGen = 1; p.Fps = 0; p.TitlePool = 17;
        var fields = ProfileValidator.Validate(p).Where(i => i.Blocking).Select(i => i.Field).ToHashSet();
        Assert.Contains(nameof(LaunchProfile.WideThreads), fields);
        Assert.Contains(nameof(LaunchProfile.TargetRoomMb), fields);
        Assert.Contains(nameof(LaunchProfile.FrameGen), fields);
        Assert.Contains(nameof(LaunchProfile.Fps), fields);
        Assert.Contains(nameof(LaunchProfile.TitlePool), fields);
    }

    [Fact]
    public void Series_s_with_dlaa_is_told_about_the_title_pool()
    {
        using var d = new TempDir();
        var p = Valid(d);
        (p.Console, p.Dlss) = (ConsoleModel.SeriesS, DlssMode.Dlaa);
        Assert.Contains(ProfileValidator.Validate(p), i => i.Field == nameof(LaunchProfile.TitlePool) && !i.Blocking);
        p.TitlePool = 2;
        Assert.DoesNotContain(ProfileValidator.Validate(p), i => i.Field == nameof(LaunchProfile.TitlePool));
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
        p.Fps = 60;
        var backup = store.Save(p);
        Assert.NotNull(backup);
        Assert.Equal(1000, ProfileStore.Read(backup!).Fps);
        Assert.Equal(60, store.Load("Mine").Fps);
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
        (p.Console, p.Dlss) = (ConsoleModel.SeriesS, DlssMode.Balanced);
        store.Save(p);
        var text = File.ReadAllText(store.PathOf(p.Name));
        Assert.Contains("\"SeriesS\"", text);
        Assert.Contains("\"Balanced\"", text);
        Assert.Contains("\"SchemaVersion\": 2", text);
    }

    /// <summary>The user's own profiles from the old base (schema 1, written by PowerShell), read as they are on disk.</summary>
    private const string StadProfile = """
        {
            "SchemaVersion":  1,
            "Name":  "Stad (768p, 2K)",
            "GameExe":  "C:\\Games\\Xbox.v1.0.2.1\\game_scarlett_master.exe",
            "CacheDir":  null,
            "DataDir":  "C:\\Users\\Gusta\\AppData\\Local\\victory\\data-city",
            "Console":  "SeriesS",
            "Resolution":  768,
            "Quality":  false,
            "RayTracing":  false,
            "Adapter":  "",
            "Build":  "Diag",
            "SmoothMotionCopy":  false,
            "WarmLimit":  0,
            "Air":  0.3,
            "Steady":  0.7,
            "Ambient":  0.8,
            "SkipBvh":  true,
            "SkipHairSim":  false,
            "LeaveOut":  true,
            "PipelineWaitMs":  null,
            "MipDrop":  0,
            "MipDropMinSize":  null,
            "WideThreads":  null,
            "TargetRoomMb":  null,
            "PoolDiscard":  null,
            "PipelineBook":  null,
            "D3D12BackgroundOff":  false,
            "Dred":  "Off",
            "Panel":  false,
            "LogFilter":  "info",
            "OutputHeight":  1080,
            "PresentScale":  true,
            "PresentSharpen":  0.3,
            "Window":  "Borderless",
            "Monitor":  0,
            "Upscaler":  "Dlss"
        }
        """;

    [Fact]
    public void An_old_profile_becomes_its_nearest_play_options()
    {
        using var d = new TempDir();
        var f = Path.Combine(d.Path, "Stad (768p, 2K).json");
        File.WriteAllText(f, StadProfile);
        var p = ProfileStore.Read(f);
        // Series S at 768 of 1080 lines with DLSS: DLSS Quality (720 lines), full screen from "Borderless".
        Assert.Equal((ConsoleModel.SeriesS, DlssMode.Quality, true), (p.Console, p.Dlss, p.Fullscreen));
        Assert.Equal(@"C:\Users\Gusta\AppData\Local\victory\data-city", p.DataDir);
        Assert.Equal((0, 1000, false, 0), (p.FrameGen, p.Fps, p.FenceSleep, p.TitlePool));
        Assert.Equal(LaunchProfile.CurrentSchema, p.SchemaVersion);
        Assert.Equal(["play", @"C:\Games\Xbox.v1.0.2.1\game_scarlett_master.exe", "--series-s", "--res", "1440", "--dlss", "quality", "--fg-type", "auto", "--fps", "1000"],
            PlanBuilder.PlayArgs(p));
        var o = PlanBuilder.Overrides(p);
        Assert.Equal("1", o["VICTORY_FULLSCREEN"]);
        // The mouse camera (the stand-in pad's right stick), as on the old base; no key map: the defaults.
        Assert.Equal("1", o["VICTORY_MOUSE"]);
        // The old base's picture switches do not reach upstream's renderer.
        foreach (var gone in new[] { "VICTORY_AMBIENT_AIR", "VICTORY_PRESENT_SCALE", "VICTORY_OWN_TAAU", "VICTORY_TEXTURE_MIP_DROP", "VICTORY_KEYMAP", "VICTORY_MOUSE_CAPTURE", "VICTORY_MOUSE_SENSE" })
            Assert.False(o.ContainsKey(gone), gone);
        Assert.Equal(@"C:\Users\Gusta\AppData\Local\victory\data-city", PlanBuilder.Paths(p)["VICTORY_DATA_DIR"]);
        // Saved again, it stays what it became.
        var store = new ProfileStore(d.Path);
        store.Save(p);
        Assert.Equal(DlssMode.Quality, store.Load(p.Name).Dlss);
        p.Dlss = DlssMode.Off;
        store.Save(p);
        Assert.Equal(DlssMode.Off, store.Load(p.Name).Dlss);
    }

    [Theory]
    [InlineData(1080, 1080, DlssMode.Dlaa)]
    [InlineData(768, 1080, DlssMode.Quality)]
    [InlineData(720, 1080, DlssMode.Quality)]
    [InlineData(630, 1080, DlssMode.Balanced)]
    [InlineData(540, 1080, DlssMode.Performance)]
    [InlineData(480, 1080, DlssMode.Performance)]
    [InlineData(1080, 1440, DlssMode.Quality)]
    public void Old_render_heights_map_to_the_nearest_dlss_mode(int height, int output, DlssMode mode) =>
        Assert.Equal(mode, LaunchProfile.NearestDlss(height, output));

    [Fact]
    public void An_old_profile_without_dlss_plays_without_it()
    {
        using var d = new TempDir();
        var f = Path.Combine(d.Path, "old.json");
        File.WriteAllText(f, """{ "SchemaVersion": 1, "Name": "old", "Console": "SeriesS", "Resolution": 540, "Window": "Windowed" }""");
        var p = ProfileStore.Read(f);
        Assert.Equal((DlssMode.Off, false), (p.Dlss, p.Fullscreen));
        Assert.Contains("off", PlanBuilder.PlayArgs(p));
    }
}

public class PresetTests
{
    [Fact]
    public void Presets_keep_the_users_game_folders_card_and_controls()
    {
        var mine = new LaunchProfile { GameExe = @"D:\g\game_scarlett_master.exe", DataDir = @"D:\d", CacheDir = @"D:\c", Adapter = "#1", Build = BuildKind.Release, FenceSleep = true, Mouse = false, Fullscreen = true };
        foreach (var preset in Presets.All)
        {
            var p = Presets.Make(preset, mine);
            Assert.Equal((mine.GameExe, mine.DataDir, mine.CacheDir, mine.Adapter), (p.GameExe, p.DataDir, p.CacheDir, p.Adapter));
            Assert.Equal((false, true), (p.Mouse, p.Fullscreen));
            Assert.False(p.FenceSleep);   // the rest starts from victory play's set
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
        Assert.Equal(["play", "x.exe", "--series-s", "--res", "1440", "--dlss", "quality", "--fg-type", "auto", "--fps", "1000"], PlanBuilder.PlayArgs(Presets.Make(Presets.ById("standard")!, basis)));
        Assert.Equal(DlssMode.Performance, Presets.Make(Presets.ById("fps")!, basis).Dlss);
        Assert.Equal(2, Presets.Make(Presets.ById("framegen")!, basis).FrameGen);
        Assert.DoesNotContain("--series-s", PlanBuilder.PlayArgs(Presets.Make(Presets.ById("seriesx")!, basis)));
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
    public void A_12_gb_card_keeps_victory_plays_set()
    {
        var preset = Presets.ForThisPc([Card(0, "NVIDIA GeForce RTX 4070 SUPER", 12)], Pc(16, 32), null);
        var p = Presets.Make(preset, new LaunchProfile { GameExe = "x.exe" });
        Assert.Equal(PlanBuilder.PlayArgs(new LaunchProfile { GameExe = "x.exe" }), PlanBuilder.PlayArgs(p));
        Assert.Contains("12 GB", preset.Description);
    }

    [Fact]
    public void A_small_card_gets_dlss_performance()
    {
        var p = Presets.Make(Presets.ForThisPc([Card(0, "NVIDIA GeForce RTX 3050", 6)], Pc(8, 16), null), new LaunchProfile());
        Assert.Equal(DlssMode.Performance, p.Dlss);
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
    public void Play_options_follow_the_profile()
    {
        var p = new LaunchProfile
        {
            GameExe = @"D:\g\game_scarlett_master.exe", Console = ConsoleModel.SeriesX, Picture = SeriesXPicture.P1080To1440,
            Dlss = DlssMode.Balanced, FrameGen = 3, FgType = FrameGenType.Dlss, Fps = 60,
        };
        Assert.Equal(["play", @"D:\g\game_scarlett_master.exe", "--res", "1080to1440", "--dlss", "balanced", "--framegen", "3", "--fg-type", "dlss", "--fps", "60"],
            PlanBuilder.PlayArgs(p));
    }

    [Fact]
    public void Warm_up_steps_are_victorys_own_commands()
    {
        var p = new LaunchProfile { GameExe = @"D:\g\game_scarlett_master.exe", WarmLimit = 50 };
        Assert.Equal(["warm", "--used", "--limit", "50"], PlanBuilder.ModeArgs(p, RunMode.Warm));
        // The full warm-up: every recorded pipeline.
        p.WarmAll = true;
        Assert.Equal(["warm", "--limit", "50"], PlanBuilder.ModeArgs(p, RunMode.Warm));
        Assert.Equal(["shaders", @"D:\g"], PlanBuilder.ModeArgs(p, RunMode.Precompile));
        // Chosen per warm-up: not saved with the profile.
        Assert.False(p.Clone().WarmAll);
    }

    [Fact]
    public void Mouse_choices_map_to_the_layers_switches()
    {
        var o = PlanBuilder.Overrides(new LaunchProfile { MouseSense = 1.5, MouseInvertY = true });
        Assert.Equal(("1", "1.5", "1"), (o["VICTORY_MOUSE"], o["VICTORY_MOUSE_SENSE"], o["VICTORY_MOUSE_INVERT_Y"]));
        Assert.False(o.ContainsKey("VICTORY_MOUSE_CAPTURE"));
        var native = PlanBuilder.Overrides(new LaunchProfile { Mouse = false, MouseCapture = true, MouseSense = 0.5 });
        Assert.Equal(("1", "0.5"), (native["VICTORY_MOUSE_CAPTURE"], native["VICTORY_MOUSE_SENSE"]));
        Assert.False(native.ContainsKey("VICTORY_MOUSE"));
        // Sensitivity 1 and no mouse at all set nothing.
        Assert.False(PlanBuilder.Overrides(new LaunchProfile()).ContainsKey("VICTORY_MOUSE_SENSE"));
        Assert.Empty(PlanBuilder.Overrides(new LaunchProfile { Mouse = false, MouseSense = 3 }));
        Assert.Contains(ProfileValidator.Validate(new LaunchProfile { MouseSense = 20 }), i => i.Field == nameof(LaunchProfile.MouseSense));
    }

    [Fact]
    public void A_plain_profile_sets_nothing_over_victory_play()
    {
        var o = PlanBuilder.Overrides(new LaunchProfile { Mouse = false });
        Assert.Empty(o);
        // This fork's switches only when asked.
        var mine = PlanBuilder.Overrides(new LaunchProfile { Mouse = false, FenceSleep = true, TitlePool = 2, TitlePriorityCap = 0 });
        Assert.Equal(("1", "2", "0"), (mine["VICTORY_FENCE_SLEEP"], mine["VICTORY_TITLE_POOL"], mine["VICTORY_TITLE_PRIORITY_CAP"]));
    }

    private static ScriptPreset Printed() => new()
    {
        Victory = @"C:\r\target\release\victory.exe", Exe = @"D:\g\game_scarlett_master.exe", Console = "series-x",
        Args = ["run", @"D:\g\game_scarlett_master.exe", "--console", "series-x", "--execute"],
        Settings = new() { ["VICTORY_PIPELINE_WAIT_MS"] = "250", ["VICTORY_RELAY"] = "2", ["VICTORY_PANEL"] = "1" },
    };

    [Fact]
    public void Profile_switches_go_as_set_and_paths_go_in_the_environment()
    {
        var p = new LaunchProfile { GameExe = @"D:\g\game_scarlett_master.exe", PipelineWaitMs = 250, Panel = true, Mouse = false, DataDir = @"D:\city" };
        var plan = new PlanBuilder(new RepoLayout(@"C:\r")).Compose(p, RunMode.Play, Printed(), new DateTime(2026, 11, 27, 9, 0, 0));
        Assert.Equal("2", plan.Settings["VICTORY_RELAY"]);
        Assert.Equal(@"D:\g", plan.Settings["VICTORY_GAME_DIR"]);
        Assert.Equal(@"D:\city", plan.Settings["VICTORY_DATA_DIR"]);
        Assert.Equal("1", plan.Settings["NO_COLOR"]);
        Assert.Contains("VICTORY_PIPELINE_WAIT_MS", plan.Overridden);
        Assert.DoesNotContain("VICTORY_RELAY", plan.Overridden);
        Assert.EndsWith("20261127-090000-play.frames.txt", plan.FrameLogPath);
        Assert.Equal(plan.FrameLogPath, plan.Settings["VICTORY_FRAME_LOG"]);
        // `victory play` clears every VICTORY_ variable but the paths: the switches go as --set, sorted.
        Assert.Equal(["play", @"D:\g\game_scarlett_master.exe", "--series-s", "--res", "1440", "--dlss", "quality", "--fg-type", "auto", "--fps", "1000",
            "--set", "VICTORY_FRAME_LOG=" + plan.FrameLogPath, "--set", "VICTORY_PANEL=1", "--set", "VICTORY_PIPELINE_WAIT_MS=250"], plan.Args);
        // A benchmark's switches join them.
        var bench = plan.With(new Dictionary<string, string> { ["VICTORY_KEYS"] = "30:Enter" }, @"C:\l\b.log", @"C:\l\b.frames.txt");
        Assert.Contains("VICTORY_KEYS=30:Enter", bench.Args);
        Assert.Contains(@"VICTORY_FRAME_LOG=C:\l\b.frames.txt", bench.Args);
        Assert.DoesNotContain("VICTORY_FRAME_LOG=" + plan.FrameLogPath, bench.Args);
    }

    [Fact]
    public void A_warm_up_gets_plays_settings_in_the_environment_and_no_set()
    {
        var plan = new PlanBuilder(new RepoLayout(@"C:\r")).Compose(new LaunchProfile { GameExe = @"D:\g\x.exe", FenceSleep = true }, RunMode.Warm, Printed(), DateTime.Now);
        Assert.Equal(["warm", "--used"], plan.Args);
        Assert.Equal("2", plan.Settings["VICTORY_RELAY"]);
        Assert.Null(plan.FrameLogPath);
        Assert.Equal(@"C:\r\target\release\victory.exe", plan.Victory);
    }

    [Fact]
    public void Errors_come_through_as_plain_text()
    {
        var e = Assert.Throws<PlanException>(() => PlanBuilder.ParsePreset("", "Error: no D:\\none\\game_scarlett_master.exe"));
        Assert.Contains("no D:\\none", e.Message);
    }

    [Fact]
    public void A_build_that_is_there_is_used()
    {
        using var d = new TempDir();
        var release = Path.Combine(d.Path, "target", "release", "victory.exe");
        Directory.CreateDirectory(Path.GetDirectoryName(release)!);
        File.WriteAllText(release, "");
        var repo = new RepoLayout(d.Path);
        Assert.Equal(release, repo.VictoryExe(BuildKind.Diag));   // an old profile's diag, with only release built
        Assert.Equal(release, repo.VictoryExe(BuildKind.Release));
    }

    /// <summary>The real victory.exe, asked as the launcher asks it: victory play's settings come back with the profile's.</summary>
    [Fact]
    public async Task The_real_victory_play_prints_its_settings()
    {
        var repo = RepoLayout.Find(AppContext.BaseDirectory);
        Assert.NotNull(repo);
        if (!File.Exists(repo!.VictoryExe(BuildKind.Release))) return;   // nothing built on this machine
        using var d = new TempDir();
        var game = Path.Combine(d.Path, "game_scarlett_master.exe");
        File.WriteAllText(game, "");
        var b = new PlanBuilder(repo);
        var plain = await b.ResolveAsync(new LaunchProfile { GameExe = game, Build = BuildKind.Release }, RunMode.Play);
        Assert.Equal("2", plain.Settings["VICTORY_RELAY"]);
        Assert.Equal("quality", plain.Settings["VICTORY_MENU_DLSS"] is "off" ? "quality" : plain.Settings["VICTORY_DLSS_MODE"]);
        Assert.Equal(["run", game, "--console", "series-s", "--execute"], plain.Args);
        Assert.False(plain.Settings.ContainsKey("VICTORY_FENCE_SLEEP"));
        var mine = await b.ResolveAsync(new LaunchProfile { GameExe = game, Build = BuildKind.Release, Console = ConsoleModel.SeriesS, FenceSleep = true, Fps = 60 }, RunMode.Play);
        Assert.Equal("series-s", mine.Console);
        Assert.Equal("1", mine.Settings["VICTORY_FENCE_SLEEP"]);
        Assert.Equal("16667", mine.Settings["VICTORY_FRAME_INTERVAL_US"]);
        Assert.Contains("--set 'VICTORY_FENCE_SLEEP=1'", mine.Settings["VICTORY_MENU_ARGS"]);   // the restarts keep it
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
        Assert.Contains("VICTORY_RELAY=2", log);           // victory play's set reached the run
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
            BaseArgs = ["-n", "60", "127.0.0.1"], Sets = new Dictionary<string, string>(), Settings = new Dictionary<string, string>(), Overridden = [],
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