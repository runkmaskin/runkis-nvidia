using System.Globalization;

namespace Runkis.Launcher.Core;

/// <summary>One key the layer holds: from <see cref="From"/> to <see cref="To"/> seconds on the script's clock.</summary>
public sealed record KeyPress(double From, double To, string Key, byte VirtualKey)
{
    /// <summary>A range whose end is not after its start is never held (the layer tests <c>from..to</c>).</summary>
    public bool Empty => To <= From;
}

/// <summary>An entry the layer would skip without a word, and why.</summary>
public sealed record SkippedEntry(string Text, string Why);

public sealed record KeyScriptParse(IReadOnlyList<KeyPress> Keys, IReadOnlyList<SkippedEntry> Skipped);

public sealed record KeyScriptCheck(IReadOnlyList<string> Errors, IReadOnlyList<string> Warnings, IReadOnlyList<string> Notes)
{
    public bool Ok => Errors.Count == 0;
}

/// <summary>
/// <c>VICTORY_KEYS</c>, read the way the layer reads it (crates/victory-hle/src/modules/gameinput.rs, <c>scripted()</c>):
/// entries split by commas, each <c>&lt;seconds&gt;:&lt;key&gt;</c> (held 0.2 s) or <c>&lt;from&gt;-&lt;to&gt;:&lt;key&gt;</c>
/// (held from..to, the end excluded). Seconds count from the first time the title opens its input; a key is a letter, a
/// digit, or Enter, Esc, Tab, Back, Up, Down, Left, Right. The layer skips any entry it cannot read; here such an entry
/// is an error, so a benchmark never runs a script that silently lost keys.
/// </summary>
public static class KeyScript
{
    public const string Variable = "VICTORY_KEYS";
    /// <summary>How long a key given one time is held (gameinput.rs: <c>Duration::from_millis(200)</c>).</summary>
    public const double TapSeconds = 0.2;

    private static readonly Dictionary<string, byte> Named = new(StringComparer.Ordinal)
    {
        ["Enter"] = 0x0D, ["Esc"] = 0x1B, ["Tab"] = 0x09, ["Back"] = 0x08,
        ["Up"] = 0x26, ["Down"] = 0x28, ["Left"] = 0x25, ["Right"] = 0x27,
    };

    /// <summary>
    /// The editor's text as the variable's value: entries may be on lines of their own or comma-separated, and
    /// <c>#</c> starts a comment (no key uses it). Empty entries are dropped.
    /// </summary>
    public static string ToVariable(string? text)
    {
        if (string.IsNullOrWhiteSpace(text)) return "";
        var entries = new List<string>();
        foreach (var raw in text.Replace("\r", "", StringComparison.Ordinal).Split('\n'))
        {
            var hash = raw.IndexOf('#', StringComparison.Ordinal);
            var line = hash >= 0 ? raw[..hash] : raw;
            entries.AddRange(line.Split(',').Select(e => e.Trim()).Where(e => e.Length > 0));
        }
        return string.Join(",", entries);
    }

    /// <summary>The value as the layer reads it.</summary>
    public static KeyScriptParse Parse(string? value)
    {
        var keys = new List<KeyPress>();
        var skipped = new List<SkippedEntry>();
        foreach (var entry in (value ?? "").Split(','))
        {
            var colon = entry.IndexOf(':', StringComparison.Ordinal);
            if (colon < 0)
            {
                if (entry.Trim().Length > 0) skipped.Add(new(entry, "no ':' between the time and the key"));
                continue;
            }
            var at = entry[..colon];
            var key = entry[(colon + 1)..].Trim();
            byte vk;
            if (Named.TryGetValue(key, out var named)) vk = named;
            else if (key.Length == 1 && char.IsAsciiLetterOrDigit(key[0])) vk = (byte)char.ToUpperInvariant(key[0]);
            else
            {
                skipped.Add(new(entry, $"'{key}' is not a key the layer knows (a letter, a digit, Enter, Esc, Tab, Back, Up, Down, Left, Right; names are case-sensitive)"));
                continue;
            }
            double? from, to;
            var dash = at.IndexOf('-', StringComparison.Ordinal);
            if (dash >= 0) (from, to) = (Seconds(at[..dash]), Seconds(at[(dash + 1)..]));
            else (from, to) = (Seconds(at), Seconds(at) + TapSeconds);
            if (from is not double f || to is not double t)
            {
                skipped.Add(new(entry, $"'{at.Trim()}' is not a time in seconds (a number, or from-to)"));
                continue;
            }
            keys.Add(new(f, t, Named.ContainsKey(key) ? key : key.ToUpperInvariant(), vk));
        }
        return new(keys, skipped);
    }

    /// <summary>Rust's <c>f64::from_str</c> then <c>Duration::try_from_secs_f64</c>: finite, not negative, not too large.</summary>
    private static double? Seconds(string text)
    {
        var t = text.Trim();
        if (t.Length == 0) return null;
        if (!double.TryParse(t, NumberStyles.AllowLeadingSign | NumberStyles.AllowDecimalPoint | NumberStyles.AllowExponent,
                CultureInfo.InvariantCulture, out var v)) return null;
        if (!double.IsFinite(v) || v < 0 || v >= 1.8e19) return null;
        return v;
    }

    /// <summary>
    /// Whether the script fits a benchmark of <paramref name="warmup"/> + <paramref name="measure"/> seconds.
    /// Errors: entries the layer would skip. Warnings: keys never held, or held after the run is stopped.
    /// </summary>
    public static KeyScriptCheck Check(string? value, double warmup, double measure)
    {
        var parse = Parse(value);
        var errors = parse.Skipped.Select(s => $"'{s.Text.Trim()}': {s.Why}. The layer would skip it.").ToList();
        var warnings = new List<string>();
        var notes = new List<string>();
        var end = warmup + measure;
        foreach (var k in parse.Keys)
        {
            if (k.Empty) warnings.Add($"{Fmt(k.From)}-{Fmt(k.To)}:{k.Key} is never held: the end is not after the start.");
            else if (k.From >= end) warnings.Add($"{Fmt(k.From)}:{k.Key} comes after the run is stopped ({Fmt(warmup)} s warm-up + {Fmt(measure)} s measured = {Fmt(end)} s), so it is never pressed.");
        }
        var held = parse.Keys.Where(k => !k.Empty).ToList();
        if (held.Count > 0)
        {
            var last = held.Max(k => k.To);
            var during = held.Count(k => k.To > warmup && k.From < end);
            notes.Add($"{held.Count} key(s); the last is let go at {Fmt(last)} s. {(during == 0 ? $"None is held after {Fmt(warmup)} s, so the measured window sees no scripted input." : $"{during} {(during == 1 ? "is" : "are")} held after {Fmt(warmup)} s, in the measured window.")}");
            notes.Add("The script's clock starts when the title first opens its input; the warm-up counts from its first picture. "
                      + "Both happen during start-up, at moments the launcher cannot see; each run records when its first picture came.");
        }
        return new(errors, warnings, notes);
    }

    private static string Fmt(double v) => v.ToString("0.###", CultureInfo.InvariantCulture);
}
