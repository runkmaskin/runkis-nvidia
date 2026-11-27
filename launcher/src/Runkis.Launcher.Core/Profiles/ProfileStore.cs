using System.Text.Json;

namespace Runkis.Launcher.Core;

/// <summary>
/// Profiles as one JSON file each. Writes go to a temporary file first and replace the old one, which is kept as
/// <c>.bak</c>; deleting moves the file to a <c>deleted</c> folder instead of removing it.
/// </summary>
public sealed class ProfileStore(string folder)
{
    public string Folder { get; } = folder;

    public static string FileName(string name) => name.Trim() + ".json";

    public string PathOf(string name) => Path.Combine(Folder, FileName(name));

    public bool Exists(string name) => File.Exists(PathOf(name));

    public IReadOnlyList<string> List()
    {
        if (!Directory.Exists(Folder)) return [];
        return Directory.EnumerateFiles(Folder, "*.json")
            .Select(Path.GetFileNameWithoutExtension)
            .OfType<string>()
            .Order(StringComparer.OrdinalIgnoreCase)
            .ToList();
    }

    public LaunchProfile Load(string name) => Read(PathOf(name));

    /// <summary>Reads and checks a profile file (also for imports); throws <see cref="InvalidDataException"/> with a plain reason.</summary>
    public static LaunchProfile Read(string path)
    {
        LaunchProfile? p;
        try { p = JsonSerializer.Deserialize<LaunchProfile>(File.ReadAllText(path), LaunchProfile.Json); }
        catch (JsonException e) { throw new InvalidDataException($"{Path.GetFileName(path)} is not a valid profile: {e.Message}", e); }
        if (p == null) throw new InvalidDataException($"{Path.GetFileName(path)} is empty.");
        if (p.SchemaVersion > LaunchProfile.CurrentSchema)
            throw new InvalidDataException($"{Path.GetFileName(path)} was written by a newer launcher (schema {p.SchemaVersion}).");
        p.LogFilter = string.IsNullOrWhiteSpace(p.LogFilter) ? "info" : p.LogFilter;
        p.Adapter ??= "";
        p.GameExe ??= "";
        return p;
    }

    /// <summary>Saves <paramref name="p"/> under its name. Returns the backup of what it replaced, if anything.</summary>
    public string? Save(LaunchProfile p)
    {
        if (string.IsNullOrWhiteSpace(p.Name) || p.Name.Trim().IndexOfAny(Path.GetInvalidFileNameChars()) >= 0)
            throw new ArgumentException("The profile name cannot be a file name.");
        Directory.CreateDirectory(Folder);
        var target = PathOf(p.Name);
        var temp = target + ".tmp";
        File.WriteAllText(temp, JsonSerializer.Serialize(p, LaunchProfile.Json));
        // Read it back before it replaces anything: a half-written profile never becomes the profile.
        Read(temp);
        if (File.Exists(target))
        {
            var backup = target + ".bak";
            File.Replace(temp, target, backup, ignoreMetadataErrors: true);
            return backup;
        }
        File.Move(temp, target);
        return null;
    }

    /// <summary>Moves the profile to <c>deleted\</c>, from where it can be put back by hand.</summary>
    public string Delete(string name)
    {
        var source = PathOf(name);
        var bin = Path.Combine(Folder, "deleted");
        Directory.CreateDirectory(bin);
        var target = Path.Combine(bin, $"{name.Trim()}-{DateTime.Now:yyyyMMdd-HHmmss}.json");
        File.Move(source, target);
        return target;
    }

    /// <summary>Writes a profile to any path (export); same safety as <see cref="Save"/>.</summary>
    public static void Export(LaunchProfile p, string path)
    {
        var temp = path + ".tmp";
        File.WriteAllText(temp, JsonSerializer.Serialize(p, LaunchProfile.Json));
        Read(temp);
        File.Move(temp, path, overwrite: true);
    }

    /// <summary>A name not yet taken: "name", "name 2", "name 3", ...</summary>
    public string FreeName(string name)
    {
        var baseName = string.IsNullOrWhiteSpace(name) ? "Profile" : name.Trim();
        if (!Exists(baseName)) return baseName;
        for (var i = 2; ; i++)
            if (!Exists($"{baseName} {i}")) return $"{baseName} {i}";
    }
}
