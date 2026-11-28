using System.Collections.ObjectModel;
using System.Windows.Input;
using Runkis.Launcher.Core;

namespace Runkis.Launcher;

/// <summary>One row of the Controls page: a pad button or stick direction and its two keys.</summary>
public sealed class KeyRow(PadTarget target, ControlsViewModel owner) : Observable
{
    private List<string> keys = [];
    public PadTarget Target { get; } = target;
    public string Xbox => Target.Xbox;
    public string Ps5 => Target.Ps5;
    public ICommand Bind1 => new Command(() => owner.StartCapture(this, 0));
    public ICommand Bind2 => new Command(() => owner.StartCapture(this, 1));
    public ICommand Clear => new Command(() => owner.Assign(this, null, null));

    public IReadOnlyList<string> Keys => keys;
    public string Key1 => Label(0);
    public string Key2 => Label(1);

    private string Label(int slot) =>
        owner.Capturing == this && owner.CaptureSlot == slot ? "press a key…" : slot < keys.Count ? KeyMap.Display(keys[slot]) : "—";

    internal void Show(List<string> k)
    {
        keys = k;
        Refresh();
    }

    internal void Refresh()
    {
        Raise(nameof(Key1));
        Raise(nameof(Key2));
    }
}

/// <summary>
/// The Controls page: the keyboard's stand-in pad as the layer maps it (VICTORY_KEYMAP), and the mouse camera. The
/// bindings live in the profile; a map equal to the defaults is stored as null, so it follows the defaults when the
/// mouse camera is switched.
/// </summary>
public sealed class ControlsViewModel : Observable
{
    private readonly Func<LaunchProfile> profile;
    private readonly Action changed;

    public ControlsViewModel(Func<LaunchProfile> profile, Action changed)
    {
        this.profile = profile;
        this.changed = changed;
        Rows = new(KeyMap.Targets.Select(t => new KeyRow(t, this)));
        Reset = new Command(() => { CancelCapture(); profile().KeyMap = null; Load(); changed(); });
        Load();
    }

    public ObservableCollection<KeyRow> Rows { get; }
    public ICommand Reset { get; }

    public KeyRow? Capturing { get; private set; }
    public int CaptureSlot { get; private set; }
    public bool IsCapturing => Capturing != null;

    public bool Mouse
    {
        get => profile().Mouse;
        set
        {
            if (profile().Mouse == value) return;
            var keep = profile().KeyMap == null ? null : Current();
            profile().Mouse = value;
            // A custom map stays as it is; the defaults follow the mouse camera.
            profile().KeyMap = keep == null ? null : Store(keep);
            Raise();
            Load();
            changed();
        }
    }

    public double MouseSense
    {
        get => profile().MouseSense;
        set { profile().MouseSense = value; Raise(); changed(); }
    }

    public bool InvertY
    {
        get => profile().MouseInvertY;
        set { profile().MouseInvertY = value; Raise(); changed(); }
    }

    /// <summary>Upstream's native mouse (VICTORY_MOUSE_CAPTURE).</summary>
    public bool Capture
    {
        get => profile().MouseCapture;
        set { profile().MouseCapture = value; Raise(); changed(); }
    }

    public string Summary => KeyMap.Variable(profile()) is { } v
        ? $"Changed from the defaults: the run gets VICTORY_KEYMAP={v}"
        : "The emulator's defaults: no VICTORY_KEYMAP is set.";

    /// <summary>Unknown names (a hand-edited profile) and keys bound twice.</summary>
    public string Issues => string.Join("\n", KeyMap.Check(profile()).Select(i => (i.Blocking ? "✕ " : "! ") + i.Message));

    /// <summary>Shows the profile's bindings (after a profile switch, too).</summary>
    public void Load()
    {
        var map = KeyMap.Effective(profile());
        foreach (var r in Rows) r.Show(map[r.Target.Id]);
        Raise(nameof(Mouse));
        Raise(nameof(MouseSense));
        Raise(nameof(InvertY));
        Raise(nameof(Capture));
        Raise(nameof(Summary));
        Raise(nameof(Issues));
    }

    internal void StartCapture(KeyRow row, int slot)
    {
        var was = Capturing;
        Capturing = row;
        CaptureSlot = slot;
        was?.Refresh();
        row.Refresh();
        Raise(nameof(IsCapturing));
    }

    public void CancelCapture()
    {
        var was = Capturing;
        Capturing = null;
        was?.Refresh();
        Raise(nameof(IsCapturing));
    }

    /// <summary>The key pressed while a row waits for one (a layer key name).</summary>
    public void Captured(string key)
    {
        if (Capturing is not { } row) return;
        var keys = row.Keys.ToList();
        // The new key in the slot clicked; the other slot keeps its key unless it is the same one.
        var (first, second) = CaptureSlot == 0 ? (key, keys.ElementAtOrDefault(1)) : (keys.ElementAtOrDefault(0), key);
        if (first == second) second = null;
        Assign(row, first ?? second, first == null ? null : second);
    }

    internal void Assign(KeyRow row, string? first, string? second)
    {
        CancelCapture();
        var map = Current();
        map[row.Target.Id] = new[] { first, second }.OfType<string>().ToList();
        profile().KeyMap = Store(map);
        Load();
        changed();
    }

    private Dictionary<string, List<string>> Current() => Rows.ToDictionary(r => r.Target.Id, r => r.Keys.ToList());

    /// <summary>The map as stored: null when it is the defaults.</summary>
    private Dictionary<string, string>? Store(Dictionary<string, List<string>> map)
    {
        var stored = map.ToDictionary(kv => kv.Key, kv => KeyMap.Join(kv.Value));
        var p = new LaunchProfile { Mouse = profile().Mouse, KeyMap = stored };
        return KeyMap.Variable(p) == null ? null : stored;
    }
}
