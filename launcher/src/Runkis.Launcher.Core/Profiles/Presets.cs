namespace Runkis.Launcher.Core;

/// <summary>A ready-made way to start, applied over `victory play`'s own set. Each one says what it is based on.</summary>
public sealed record Preset(string Id, string Name, string Description, Action<LaunchProfile> Apply);

public static class Presets
{
    public static readonly IReadOnlyList<Preset> All =
    [
        new("standard", "Standard",
            "victory play's set on the Series S profile (1080p output, a 720-line scene with DLSS Quality), no frame rate cap, "
            + "frame generation ready but off (F1 in the game switches it).",
            _ => { }),
        new("framegen", "Frame generation",
            "Standard with DLSS frame generation on from the start, two frames shown for each one rendered (RTX 40 and later; "
            + "AMD FSR 3 on other cards).",
            p => { p.FrameGen = 2; }),
        new("fps", "Faster",
            "DLSS Performance (a 540-line scene on Series S): softer, lighter on the card.",
            p => { p.Dlss = DlssMode.Performance; }),
        new("seriesx", "Series X",
            "victory play's own default: the larger console profile at 2560x1440 with DLSS Quality. More detail, slower.",
            p => { p.Console = ConsoleModel.SeriesX; p.Dlss = DlssMode.Quality; }),
        new("safe", "Troubleshoot",
            "For crashes and GPU resets: GPU crash data in the log (diag build) and the stats panel. Slow; use it to find a "
            + "problem, not to play.",
            p => { p.Dred = DredMode.Light; p.Build = BuildKind.Diag; p.Panel = true; }),
    ];

    public static Preset? ById(string? id) => All.FirstOrDefault(p => p.Id == id);

    /// <summary>
    /// A preset worked out from this PC's card, CPU and RAM: `victory play`'s set, on the NVIDIA card with the most
    /// memory, and on cards with less than 8 GB DLSS Performance. Not measured on the PC it is made for: compare it
    /// with Standard.
    /// </summary>
    public static Preset ForThisPc(IReadOnlyList<GpuAdapter> adapters, SystemSnapshot sys, string? adapterSetting)
    {
        var why = new List<string>();
        var (chosen, _) = Dxgi.Resolve(adapters, adapterSetting);
        var best = adapters.Where(a => !a.Software && a.IsNvidia).OrderByDescending(a => a.DedicatedVideoMemory).FirstOrDefault();
        string? pick = null;
        if (string.IsNullOrWhiteSpace(adapterSetting) && best != null && chosen?.Index != best.Index)
        {
            pick = $"#{best.Index}";
            chosen = best;
            why.Add($"runs on {best.Name} (#{best.Index}), not Windows' default card");
        }
        var gb = (chosen?.DedicatedVideoMemory ?? 0) / (double)(1UL << 30);
        DlssMode? dlss = null;
        if (gb > 0 && gb < 7.5)
        {
            dlss = DlssMode.Performance;
            why.Add($"{gb:0.#} GB of video memory: DLSS Performance, a smaller scene");
        }
        else if (gb > 0)
            why.Add($"{gb:0} GB of video memory: victory play's set as it is");
        if (sys.LogicalProcessors < 12)
            why.Add("fewer than 12 CPU threads: expect lower fps; the game is CPU-bound");
        if (chosen != null && !chosen.IsNvidia)
            why.Add($"{chosen.Name} is not an NVIDIA card: this build is tuned for NVIDIA (FSR 3 frame generation instead of DLSS's)");
        if (sys.TotalRam < 29.5 * (1UL << 30))
            why.Add($"{sys.TotalRam / (double)(1UL << 30):0} GB RAM: below the 32 GB victory play asks for");

        var description = "Worked out for this PC (not measured here; compare it with Standard): "
                          + (why.Count > 0 ? string.Join("; ", why) + "." : "victory play's set fits it as it is.");
        return new("pc", "Recommended for this PC", description, p =>
        {
            if (pick != null && string.IsNullOrWhiteSpace(p.Adapter)) p.Adapter = pick;
            if (dlss is DlssMode d) p.Dlss = d;
        });
    }

    /// <summary>
    /// The preset as a profile: the user's game, folders, card, build, log filter and controls kept from
    /// <paramref name="basis"/>, everything else from `victory play`'s defaults and the preset.
    /// </summary>
    public static LaunchProfile Make(Preset preset, LaunchProfile basis)
    {
        var p = new LaunchProfile
        {
            Name = preset.Name,
            GameExe = basis.GameExe,
            CacheDir = basis.CacheDir,
            DataDir = basis.DataDir,
            Adapter = basis.Adapter,
            Build = basis.Build,
            LogFilter = basis.LogFilter,
            WarmLimit = basis.WarmLimit,
            // The controls and the window are the player's, not a speed choice.
            Mouse = basis.Mouse,
            MouseCapture = basis.MouseCapture,
            MouseSense = basis.MouseSense,
            MouseInvertY = basis.MouseInvertY,
            KeyMap = basis.KeyMap == null ? null : new(basis.KeyMap),
            Fullscreen = basis.Fullscreen,
            Fps = basis.Fps,
        };
        preset.Apply(p);
        return p;
    }
}