namespace Runkis.Launcher.Core;

/// <summary>One thing a key can drive on the keyboard's stand-in pad: its name in VICTORY_KEYMAP and on both pads.</summary>
public sealed record PadTarget(string Id, string Xbox, string Ps5);

/// <summary>
/// The keyboard's stand-in pad, as VICTORY_KEYMAP gives it to the layer (gameinput.rs, <c>Keymap::parse</c>). The
/// names here are the layer's; a test reads the layer's own lists and checks they match.
/// </summary>
public static class KeyMap
{
    /// <summary>Every target, in the layer's order (<c>TARGET_NAMES</c>).</summary>
    public static readonly IReadOnlyList<PadTarget> Targets =
    [
        new("A", "A", "Cross"),
        new("B", "B", "Circle"),
        new("X", "X", "Square"),
        new("Y", "Y", "Triangle"),
        new("LB", "LB", "L1"),
        new("RB", "RB", "R1"),
        new("LT", "LT", "L2"),
        new("RT", "RT", "R2"),
        new("View", "View", "Create"),
        new("Menu", "Menu", "Options"),
        new("LS", "LS click", "L3"),
        new("RS", "RS click", "R3"),
        new("Up", "D-pad up", "D-pad up"),
        new("Down", "D-pad down", "D-pad down"),
        new("Left", "D-pad left", "D-pad left"),
        new("Right", "D-pad right", "D-pad right"),
        new("LSUp", "Left stick up", "Left stick up"),
        new("LSDown", "Left stick down", "Left stick down"),
        new("LSLeft", "Left stick left", "Left stick left"),
        new("LSRight", "Left stick right", "Left stick right"),
        new("RSUp", "Right stick up", "Right stick up"),
        new("RSDown", "Right stick down", "Right stick down"),
        new("RSLeft", "Right stick left", "Right stick left"),
        new("RSRight", "Right stick right", "Right stick right"),
    ];

    /// <summary>The layer's key names besides letters and digits (<c>KEY_NAMES</c>). Shift, Ctrl and Alt are the left ones.</summary>
    public static readonly IReadOnlyList<string> NamedKeys =
    [
        "Space", "Enter", "Esc", "Tab", "Back", "Shift", "Ctrl", "Alt", "Up", "Down", "Left", "Right",
        "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12",
        "Mouse1", "Mouse2", "Mouse3", "Mouse4", "Mouse5",
    ];

    /// <summary>Every key name the layer reads: A-Z, 0-9 and <see cref="NamedKeys"/>.</summary>
    public static readonly IReadOnlyList<string> KeyNames =
        [.. Enumerable.Range('A', 26).Select(c => ((char)c).ToString()), .. Enumerable.Range('0', 10).Select(c => ((char)c).ToString()), .. NamedKeys];

    private static readonly Dictionary<string, string[]> Keyboard = new()
    {
        ["A"] = ["Enter"], ["B"] = ["Back"], ["X"] = ["X"], ["Y"] = ["Y"], ["LB"] = ["Q"], ["RB"] = ["E"],
        ["LT"] = ["Z"], ["RT"] = ["C"], ["View"] = ["Tab"], ["Menu"] = ["Esc"], ["LS"] = ["F"], ["RS"] = ["H"],
        ["Up"] = ["Up"], ["Down"] = ["Down"], ["Left"] = ["Left"], ["Right"] = ["Right"],
        ["LSUp"] = ["W"], ["LSDown"] = ["S"], ["LSLeft"] = ["A"], ["LSRight"] = ["D"],
        ["RSUp"] = ["I"], ["RSDown"] = ["K"], ["RSLeft"] = ["J"], ["RSRight"] = ["L"],
    };

    /// <summary>The layer's defaults; with the mouse camera, its buttons join them (left fires, right aims, middle clicks the stick).</summary>
    public static Dictionary<string, List<string>> Defaults(bool mouse)
    {
        var d = Keyboard.ToDictionary(kv => kv.Key, kv => kv.Value.ToList());
        if (mouse)
        {
            d["LT"].Add("Mouse2");
            d["RT"].Add("Mouse1");
            d["RS"].Add("Mouse3");
        }
        return d;
    }

    /// <summary>A stored value ("C|Mouse1", "" for none) as its keys.</summary>
    public static List<string> Split(string? value) =>
        (value ?? "").Split('|', StringSplitOptions.TrimEntries | StringSplitOptions.RemoveEmptyEntries).ToList();

    public static string Join(IEnumerable<string> keys) => string.Join("|", keys);

    /// <summary>The bindings in force for a profile: its own map over the defaults; a target it lacks keeps its default.</summary>
    public static Dictionary<string, List<string>> Effective(LaunchProfile p)
    {
        var d = Defaults(p.Mouse);
        if (p.KeyMap != null)
            foreach (var t in Targets)
                if (p.KeyMap.TryGetValue(t.Id, out var v)) d[t.Id] = Split(v);
        return d;
    }

    /// <summary>
    /// VICTORY_KEYMAP for the profile, or null when its bindings are the layer's defaults (then the layer keeps
    /// exactly its old mapping). Every target is written, in the layer's order, so the result does not depend on the
    /// layer's defaults.
    /// </summary>
    public static string? Variable(LaunchProfile p)
    {
        var map = Effective(p);
        var defaults = Defaults(p.Mouse);
        if (Targets.All(t => Same(map[t.Id], defaults[t.Id]))) return null;
        return string.Join(",", Targets.Select(t => $"{t.Id}={Join(map[t.Id])}"));
    }

    private static bool Same(List<string> a, List<string> b) =>
        a.Count == b.Count && a.All(k => b.Contains(k, StringComparer.OrdinalIgnoreCase));

    public static bool IsKey(string name) => KeyNames.Contains(name, StringComparer.OrdinalIgnoreCase);

    /// <summary>Problems with a stored map: unknown names are refused, a key on two targets is only a warning (it presses both).</summary>
    public static IEnumerable<ValidationIssue> Check(LaunchProfile p)
    {
        if (p.KeyMap == null) yield break;
        foreach (var (target, value) in p.KeyMap)
        {
            if (!Targets.Any(t => t.Id == target)) yield return new(nameof(LaunchProfile.KeyMap), $"'{target}' is not a pad button or stick direction.");
            foreach (var k in Split(value).Where(k => !IsKey(k)))
                yield return new(nameof(LaunchProfile.KeyMap), $"'{k}' (for {target}) is not a key the emulator knows.");
        }
        var map = Effective(p);
        var twice = Targets.SelectMany(t => map[t.Id].Select(k => (Key: k, Target: t.Xbox)))
            .GroupBy(x => x.Key, StringComparer.OrdinalIgnoreCase)
            .Where(g => g.Count() > 1);
        foreach (var g in twice)
            yield return new(nameof(LaunchProfile.KeyMap), $"{Display(g.Key)} is bound to {string.Join(" and ", g.Select(x => x.Target))}: it presses all of them.", Blocking: false);
    }

    /// <summary>A key name for people: the mouse buttons and Backspace spelt out.</summary>
    public static string Display(string name) => name switch
    {
        "Mouse1" => "Left mouse",
        "Mouse2" => "Right mouse",
        "Mouse3" => "Middle mouse",
        "Mouse4" => "Mouse back",
        "Mouse5" => "Mouse forward",
        "Back" => "Backspace",
        _ => name,
    };
}
