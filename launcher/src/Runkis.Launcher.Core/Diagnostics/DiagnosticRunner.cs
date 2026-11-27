using System.Diagnostics;
using System.Text;

namespace Runkis.Launcher.Core;

public enum CheckStatus { Pass, Info, Warning, Fail }

public sealed record CheckResult(string Title, CheckStatus Status, string Detail, string? Fix = null);

/// <summary>Checks made before a run. Each one says what it found, why it matters, and what to do.</summary>
public sealed class DiagnosticRunner(RepoLayout? repo)
{
    private const ulong GB = 1UL << 30;

    public async Task<List<CheckResult>> RunAsync(LaunchProfile p, CancellationToken ct = default)
    {
        var r = new List<CheckResult>();
        var sys = SystemInfo.Read();
        var adapters = Dxgi.Adapters();

        // ---- The source tree and the build ----
        if (repo == null)
        {
            r.Add(new("Project folder", CheckStatus.Fail, "The runkis-nvidia folder (with .victory-root and scripts\\play-nvidia.ps1) was not found.",
                "Choose it in Settings > Project folder."));
            return r;
        }
        r.Add(new("Project folder", CheckStatus.Pass, repo.Root));
        var exe = repo.VictoryExe(p.Build);
        if (!File.Exists(exe))
            r.Add(new("Emulator build", CheckStatus.Fail, $"{exe} does not exist.", $"Build it: {RepoLayout.BuildCommand(p.Build)}"));
        else
        {
            var built = File.GetLastWriteTimeUtc(exe);
            var newest = NewestSource(repo.Root);
            if (newest is { } n && n.Time > built.AddSeconds(2))
                r.Add(new("Emulator build", CheckStatus.Warning,
                    $"{Path.GetFileName(Path.GetDirectoryName(exe))}\\victory.exe was built {built.ToLocalTime():g}, but {n.File} changed after it ({n.Time.ToLocalTime():g}).",
                    $"Rebuild so the run has the newest code: {RepoLayout.BuildCommand(p.Build)}"));
            else
                r.Add(new("Emulator build", CheckStatus.Pass, $"{exe} ({new FileInfo(exe).Length / 1048576} MB, built {built.ToLocalTime():g})"));
        }

        // ---- The profile ----
        foreach (var i in ProfileValidator.Validate(p, repo))
            r.Add(new($"Setting: {i.Field}", i.Blocking ? CheckStatus.Fail : CheckStatus.Warning, i.Message));
        if (File.Exists(p.GameExe))
        {
            var name = Path.GetFileName(p.GameExe);
            r.Add(!name.Equals("game_scarlett_master.exe", StringComparison.OrdinalIgnoreCase)
                ? new("Game executable", CheckStatus.Warning, $"{name}: this build is tuned for game_scarlett_master.exe (v1.0.2.1).")
                : new("Game executable", CheckStatus.Pass, p.GameExe));
            if (repo.Contains(p.GameExe))
                r.Add(new("Game location", CheckStatus.Warning, "The game is inside the source tree. It runs, but keep it outside so it can never be committed (docs\\asset-policy.md)."));
        }

        // ---- The layer's own check of its folders (victory config; non-zero exit on a policy violation) ----
        if (File.Exists(exe) && File.Exists(p.GameExe))
            r.Add(await ConfigCheck(exe, p, ct).ConfigureAwait(false));

        // ---- Disk ----
        var cache = string.IsNullOrWhiteSpace(p.CacheDir) ? AppPaths.VictoryDefault("cache") : p.CacheDir;
        r.Add(Disk("Disk space (cache)", cache, 10, "Shaders and pipelines are cached here."));
        // The driver keeps its shader cache on the system drive; -Warm wrote about 23 GB there (README).
        r.Add(Disk("Disk space (driver cache, C:)", Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), 25,
            "The driver's shader cache lives on C:. Building the pipeline cache needs about 23 GB there."));

        // ---- Memory ----
        // A 32 GB PC reports 30-31.9 GB to Windows (firmware and an integrated GPU keep some), so 29.5 is "32 GB".
        r.Add(sys.TotalRam >= 29.5 * GB
            ? new("RAM", CheckStatus.Pass, $"{sys.TotalRam / (double)GB:0} GB")
            : new("RAM", sys.TotalRam >= 15 * GB ? CheckStatus.Warning : CheckStatus.Fail,
                $"{sys.TotalRam / (double)GB:0} GB. The game uses about 20 GB and the layer more on top; 32 GB is needed.",
                "Close everything else, and keep the paging file system-managed."));
        r.Add(sys.CommitLimit >= 48 * GB
            ? new("Commit limit (RAM + paging file)", CheckStatus.Pass, $"{sys.CommitLimit / (double)GB:0} GB")
            : new("Commit limit (RAM + paging file)", CheckStatus.Warning,
                $"{sys.CommitLimit / (double)GB:0} GB. Runs have used 40-46 GB of private memory; at the limit Windows refuses memory and the game crashes.",
                "Let Windows manage the paging file size, or set it larger."));

        // ---- The graphics card ----
        var hardware = adapters.Where(a => !a.Software).ToList();
        if (hardware.Count == 0)
            r.Add(new("Graphics card", CheckStatus.Fail, "DXGI lists no hardware graphics card."));
        var (chosen, matched) = Dxgi.Resolve(adapters, p.Adapter);
        if (!matched)
            r.Add(new("Graphics card setting", CheckStatus.Warning,
                $"'{p.Adapter}' matches no hardware card; the layer will use Windows' default ({chosen?.Name ?? "none"}).",
                "Pick the card again in Settings."));
        if (chosen != null)
        {
            var vram = chosen.DedicatedVideoMemory / (double)GB;
            if (!chosen.IsNvidia)
            {
                var nvidia = hardware.FirstOrDefault(a => a.IsNvidia);
                r.Add(nvidia != null
                    ? new("Graphics card", CheckStatus.Fail, $"The run would use {chosen.Name}, but this PC has {nvidia.Name}.",
                        $"Set the graphics card to #{nvidia.Index} ({nvidia.Name}) in Settings.")
                    : new("Graphics card", CheckStatus.Warning, $"{chosen.Name}: this build is tuned for NVIDIA RTX cards.",
                        "For other cards the plain VIctory set (scripts\\play.ps1) may suit better."));
            }
            else
                r.Add(new("Graphics card", CheckStatus.Pass,
                    $"{chosen.Name}, {vram:0.#} GB, driver {chosen.NvidiaVersion ?? chosen.DriverVersion ?? "unknown"}"));
            if (vram < 7.5 && chosen.DedicatedVideoMemory > 0)
                r.Add(new("Video memory", CheckStatus.Warning, $"{vram:0.#} GB. The settings here were measured on 8 GB; less means more texture churn.",
                    "Use 480p or 540p, and keep the texture mip drop at 1 or higher."));
        }

        // ---- Windows settings the measurements were made with (README "Test PC") ----
        r.Add(sys.HardwareScheduling switch
        {
            true => new("Hardware-accelerated GPU scheduling", CheckStatus.Pass, "On"),
            false => new("Hardware-accelerated GPU scheduling", CheckStatus.Info, "Off. The figures in the README were measured with it on.",
                "Settings > System > Display > Graphics > Change default graphics settings."),
            null => new("Hardware-accelerated GPU scheduling", CheckStatus.Info, "Not offered by this driver/Windows."),
        });
        var plan = SystemInfo.PowerPlanGuid();
        r.Add(plan == SystemInfo.HighPerformance || plan == SystemInfo.Ultimate
            ? new("Power plan", CheckStatus.Pass, sys.PowerPlan ?? "High performance")
            : new("Power plan", CheckStatus.Info, $"{sys.PowerPlan ?? "Unknown"}. The game is CPU-bound; the README's figures used High performance.",
                "Control Panel > Power Options > High performance."));
        if (sys.IsAdmin)
            r.Add(new("Running as administrator", CheckStatus.Warning,
                "The driver then keeps its shader cache in another place, so shaders built in normal runs are not found.",
                "Start the launcher normally, not as administrator."));

        // ---- Other runs ----
        var others = Process.GetProcessesByName("victory").Concat(Process.GetProcessesByName("victory_sm")).ToList();
        if (others.Count > 0)
            r.Add(new("Other emulator runs", CheckStatus.Warning, $"{others.Count} victory process(es) already running (pid {string.Join(", ", others.Select(o => o.Id))}).",
                "Stop them first: two runs share RAM, VRAM and the caches."));
        foreach (var o in others) o.Dispose();
        return r;
    }

    private static CheckResult Disk(string title, string path, int needGb, string why)
    {
        try
        {
            var root = Path.GetPathRoot(Path.GetFullPath(path));
            if (root == null) return new(title, CheckStatus.Info, "Unknown drive.");
            var d = new DriveInfo(root);
            var free = d.AvailableFreeSpace / (double)GB;
            return free >= needGb
                ? new(title, CheckStatus.Pass, $"{free:0} GB free on {root}")
                : new(title, CheckStatus.Warning, $"{free:0} GB free on {root}. {why}", $"Free at least {needGb} GB.");
        }
        catch (Exception e) when (e is IOException or ArgumentException or UnauthorizedAccessException)
        {
            return new(title, CheckStatus.Info, $"Could not read the drive: {e.Message}");
        }
    }

    /// <summary>The newest Rust source or manifest, to tell a stale build.</summary>
    internal static (string File, DateTime Time)? NewestSource(string root)
    {
        (string, DateTime)? newest = null;
        foreach (var dir in new[] { "crates", "apps" })
        {
            var d = Path.Combine(root, dir);
            if (!Directory.Exists(d)) continue;
            foreach (var f in Directory.EnumerateFiles(d, "*.*", SearchOption.AllDirectories))
            {
                if (!(f.EndsWith(".rs", StringComparison.Ordinal) || f.EndsWith("Cargo.toml", StringComparison.Ordinal))) continue;
                if (f.Contains($"{Path.DirectorySeparatorChar}target{Path.DirectorySeparatorChar}")) continue;
                var t = File.GetLastWriteTimeUtc(f);
                if (newest == null || t > newest.Value.Item2) newest = (Path.GetRelativePath(root, f), t);
            }
        }
        return newest;
    }

    private static async Task<CheckResult> ConfigCheck(string exe, LaunchProfile p, CancellationToken ct)
    {
        var psi = new ProcessStartInfo(exe, "config")
        {
            UseShellExecute = false, RedirectStandardOutput = true, RedirectStandardError = true, CreateNoWindow = true,
            StandardOutputEncoding = Encoding.UTF8,
        };
        foreach (var k in psi.Environment.Keys.Where(k => k.StartsWith("VICTORY_", StringComparison.OrdinalIgnoreCase)).ToList()) psi.Environment.Remove(k);
        psi.Environment["VICTORY_GAME_DIR"] = Path.GetDirectoryName(Path.GetFullPath(p.GameExe))!;
        if (!string.IsNullOrWhiteSpace(p.CacheDir)) psi.Environment["VICTORY_CACHE_DIR"] = p.CacheDir;
        if (!string.IsNullOrWhiteSpace(p.DataDir)) psi.Environment["VICTORY_DATA_DIR"] = p.DataDir;
        psi.WorkingDirectory = Path.GetDirectoryName(Path.GetDirectoryName(Path.GetDirectoryName(exe)))!;
        try
        {
            using var proc = Process.Start(psi)!;
            var output = await proc.StandardOutput.ReadToEndAsync(ct).ConfigureAwait(false);
            var errors = await proc.StandardError.ReadToEndAsync(ct).ConfigureAwait(false);
            await proc.WaitForExitAsync(ct).ConfigureAwait(false);
            var violations = output.Split('\n').Where(l => l.StartsWith("POLICY VIOLATION", StringComparison.Ordinal)).Select(l => l.Trim()).ToList();
            if (proc.ExitCode == 0 && violations.Count == 0)
                return new("Folders (victory config)", CheckStatus.Pass, "The layer accepts the game, cache and data folders.");
            return new("Folders (victory config)", CheckStatus.Fail,
                violations.Count > 0 ? string.Join(" ", violations) : $"victory config failed: {errors.Trim()}",
                "Choose cache and data folders outside the source tree (or leave them empty for the default).");
        }
        catch (Exception e) when (e is System.ComponentModel.Win32Exception or InvalidOperationException)
        {
            return new("Folders (victory config)", CheckStatus.Warning, $"Could not run victory config: {e.Message}");
        }
    }
}
