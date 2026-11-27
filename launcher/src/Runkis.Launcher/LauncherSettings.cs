using System.IO;
using System.Text.Json;
using Runkis.Launcher.Core;

namespace Runkis.Launcher;

/// <summary>The launcher's own choices (not the emulator's): which project folder and which profile was last used.</summary>
public sealed class LauncherSettings
{
    public string? RepoRoot { get; set; }
    public string? LastProfile { get; set; }

    public static LauncherSettings Load()
    {
        try
        {
            if (File.Exists(AppPaths.SettingsFile))
                return JsonSerializer.Deserialize<LauncherSettings>(File.ReadAllText(AppPaths.SettingsFile)) ?? new();
        }
        catch (Exception e) when (e is IOException or JsonException) { }
        return new();
    }

    public void Save()
    {
        try
        {
            Directory.CreateDirectory(AppPaths.Roaming);
            var temp = AppPaths.SettingsFile + ".tmp";
            File.WriteAllText(temp, JsonSerializer.Serialize(this, new JsonSerializerOptions { WriteIndented = true }));
            File.Move(temp, AppPaths.SettingsFile, overwrite: true);
        }
        catch (IOException) { }
    }
}
