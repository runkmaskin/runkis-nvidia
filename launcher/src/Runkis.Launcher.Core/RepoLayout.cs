namespace Runkis.Launcher.Core;

/// <summary>The source tree the launcher drives: the build outputs and the launch script live in it.</summary>
public sealed record RepoLayout(string Root)
{
    /// <summary>The marker file at the root of a VIctory source tree (crates/victory-core/src/policy.rs).</summary>
    public const string RootMarker = ".victory-root";

    /// <summary>The script that starts the title the same way from PowerShell; its presence marks the tree as this fork's.</summary>
    public string Script => Path.Combine(Root, "scripts", "play-nvidia.ps1");

    /// <summary>
    /// The build of that kind; the other one when only it is built (the old base's profiles say diag, and this base's
    /// own build is release).
    /// </summary>
    public string VictoryExe(BuildKind build)
    {
        var asked = Path.Combine(Root, "target", BuildFolder(build), "victory.exe");
        var other = Path.Combine(Root, "target", BuildFolder(build == BuildKind.Release ? BuildKind.Diag : BuildKind.Release), "victory.exe");
        return !File.Exists(asked) && File.Exists(other) ? other : asked;
    }

    public static string BuildFolder(BuildKind build) => build == BuildKind.Release ? "release" : "diag";

    /// <summary>The cargo command that makes the build the launcher looks for.</summary>
    public static string BuildCommand(BuildKind build) => build == BuildKind.Release
        ? "cargo build --release --locked -p victory"
        : "cargo build --locked --profile diag -p victory --features diagnostics";

    public bool IsValid => File.Exists(Path.Combine(Root, RootMarker)) && File.Exists(Script);

    /// <summary>Walks up from <paramref name="start"/> to the first folder that is a source tree.</summary>
    public static RepoLayout? Find(string? start)
    {
        if (string.IsNullOrWhiteSpace(start)) return null;
        DirectoryInfo? dir;
        try { dir = new DirectoryInfo(Path.GetFullPath(start)); }
        catch (Exception) { return null; }
        for (; dir != null; dir = dir.Parent)
        {
            var layout = new RepoLayout(dir.FullName);
            if (layout.IsValid) return layout;
        }
        return null;
    }

    /// <summary>Whether <paramref name="path"/> lies inside this tree, which the layer refuses for caches and saves.</summary>
    public bool Contains(string path)
    {
        try
        {
            var full = Path.GetFullPath(path).TrimEnd(Path.DirectorySeparatorChar) + Path.DirectorySeparatorChar;
            var root = Path.GetFullPath(Root).TrimEnd(Path.DirectorySeparatorChar) + Path.DirectorySeparatorChar;
            return full.StartsWith(root, StringComparison.OrdinalIgnoreCase);
        }
        catch (Exception) { return false; }
    }
}

/// <summary>Where the launcher keeps its own files (never inside the source tree).</summary>
public static class AppPaths
{
    public static string Roaming => Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData), "RunkisLauncher");
    public static string Local => Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "RunkisLauncher");
    public static string Profiles => Path.Combine(Roaming, "profiles");
    public static string Logs => Path.Combine(Local, "logs");
    public static string Benchmarks => Path.Combine(Local, "benchmarks");
    public static string SettingsFile => Path.Combine(Roaming, "launcher.json");

    /// <summary>The layer's own defaults when no cache or data folder is set (crates/victory-core/src/config.rs default_dir).</summary>
    public static string VictoryDefault(string name) =>
        Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "victory", name);
}
