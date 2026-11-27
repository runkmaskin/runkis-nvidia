using System.Text.RegularExpressions;
using Runkis.Launcher.Core;
using Xunit;

namespace Runkis.Launcher.Tests;

public class KeyMapTests
{
    /// <summary>The layer's gameinput.rs, read from the source tree the tests run in.</summary>
    private static string LayerSource()
    {
        var repo = RepoLayout.Find(AppContext.BaseDirectory);
        Assert.NotNull(repo);
        return File.ReadAllText(Path.Combine(repo!.Root, "crates", "victory-hle", "src", "modules", "gameinput.rs"));
    }

    /// <summary>The quoted names of a Rust constant list <c>const NAME: [(&amp;str, ...); n] = [ ... ];</c>.</summary>
    private static List<string> RustNames(string source, string constant)
    {
        var start = source.IndexOf($"const {constant}:", StringComparison.Ordinal);
        Assert.True(start >= 0, constant);
        var end = source.IndexOf("];", start, StringComparison.Ordinal);
        return Regex.Matches(source[start..end], "\\(\"([^\"]+)\",").Select(m => m.Groups[1].Value).ToList();
    }

    private static string RustText(string source, string constant) =>
        Regex.Match(source, $"const {constant}: &str = \"([^\"]+)\";").Groups[1].Value;

    [Fact]
    public void The_names_are_the_layers_own()
    {
        var rs = LayerSource();
        Assert.Equal(RustNames(rs, "TARGET_NAMES"), KeyMap.Targets.Select(t => t.Id));
        Assert.Equal(RustNames(rs, "KEY_NAMES"), KeyMap.NamedKeys);
        // Letters and digits are read by the layer as one character each.
        Assert.Equal(26 + 10 + KeyMap.NamedKeys.Count, KeyMap.KeyNames.Count);
        Assert.All(KeyMap.KeyNames, k => Assert.True(k.Length == 1 && char.IsAsciiLetterOrDigit(k[0]) || KeyMap.NamedKeys.Contains(k), k));
    }

    [Fact]
    public void The_defaults_are_the_layers_defaults()
    {
        var rs = LayerSource();
        string Text(bool mouse) => string.Join(",", KeyMap.Targets.Select(t => $"{t.Id}={KeyMap.Join(KeyMap.Defaults(mouse)[t.Id])}"));
        Assert.Equal(RustText(rs, "DEFAULTS_AS_TEXT"), Text(false));
        Assert.Equal(RustText(rs, "MOUSE_DEFAULTS_AS_TEXT"), Text(true));
    }

    [Fact]
    public void Defaults_set_no_variable()
    {
        Assert.False(PlanBuilder.Overrides(new LaunchProfile()).ContainsKey("VICTORY_KEYMAP"));
        Assert.False(PlanBuilder.Overrides(new LaunchProfile { Mouse = false }).ContainsKey("VICTORY_KEYMAP"));
        // A stored map that equals the defaults, in any order and case, is the defaults too.
        var same = KeyMap.Defaults(true).ToDictionary(kv => kv.Key, kv => KeyMap.Join(Enumerable.Reverse(kv.Value).Select(k => k.ToLowerInvariant())));
        Assert.Null(KeyMap.Variable(new LaunchProfile { Mouse = true, KeyMap = same }));
        Assert.Null(KeyMap.Variable(new LaunchProfile { KeyMap = new() { ["A"] = "Enter" } }));
    }

    [Fact]
    public void A_change_gives_the_whole_map_as_the_layer_reads_it()
    {
        var p = new LaunchProfile { Mouse = true, KeyMap = new() { ["A"] = "Space", ["X"] = "R", ["LS"] = "Shift", ["Y"] = "" } };
        Assert.Equal(
            "A=Space,B=Back,X=R,Y=,LB=Q,RB=E,LT=Z|Mouse2,RT=C|Mouse1,View=Tab,Menu=Esc,LS=Shift,RS=H|Mouse3,"
            + "Up=Up,Down=Down,Left=Left,Right=Right,LSUp=W,LSDown=S,LSLeft=A,LSRight=D,RSUp=I,RSDown=K,RSLeft=J,RSRight=L",
            PlanBuilder.Overrides(p)["VICTORY_KEYMAP"]);
        // Without the mouse camera the mouse buttons are not in the defaults.
        var q = new LaunchProfile { Mouse = false, KeyMap = new() { ["RT"] = "Mouse1" } };
        Assert.Contains("LT=Z,RT=Mouse1,", KeyMap.Variable(q));
    }

    [Fact]
    public void Unknown_names_are_refused_and_a_key_twice_is_a_warning()
    {
        var bad = ProfileValidator.Validate(new LaunchProfile { KeyMap = new() { ["A"] = "Space|Nope", ["Jump"] = "X" } })
            .Where(i => i.Field == nameof(LaunchProfile.KeyMap)).ToList();
        Assert.Contains(bad, i => i.Blocking && i.Message.Contains("'Nope'"));
        Assert.Contains(bad, i => i.Blocking && i.Message.Contains("'Jump'"));
        var twice = ProfileValidator.Validate(new LaunchProfile { KeyMap = new() { ["A"] = "Space", ["B"] = "Space" } })
            .Where(i => i.Field == nameof(LaunchProfile.KeyMap)).ToList();
        var warn = Assert.Single(twice);
        Assert.False(warn.Blocking);
        Assert.Contains("Space", warn.Message);
        Assert.DoesNotContain(ProfileValidator.Validate(new LaunchProfile()), i => i.Field == nameof(LaunchProfile.KeyMap));
    }

    [Fact]
    public void The_map_is_saved_with_the_profile_and_kept_by_presets()
    {
        using var d = new TempDir();
        var store = new ProfileStore(d.Path);
        store.Save(new LaunchProfile { Name = "keys", KeyMap = new() { ["A"] = "Space|Enter" } });
        Assert.Equal("Space|Enter", store.Load("keys").KeyMap!["A"]);
        Assert.Null(store.Load("keys").KeyMap!.GetValueOrDefault("B"));
        var made = Presets.Make(Presets.All[0], new LaunchProfile { Mouse = false, MouseSense = 2, KeyMap = new() { ["A"] = "Space" } });
        Assert.Equal(("Space", false, 2.0), (made.KeyMap!["A"], made.Mouse, made.MouseSense));
    }
}
