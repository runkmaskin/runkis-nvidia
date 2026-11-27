using System.Text.RegularExpressions;

namespace Runkis.Launcher.Core;

public enum Severity { Info, Warning, Error }

/// <summary>A problem recognised in the layer's output, in plain words.</summary>
public sealed record Problem(string Id, Severity Severity, string Title, string Explanation, string Fix, string Evidence);

/// <summary>
/// Recognises the layer's own messages (each rule names where it is printed) and explains them.
/// </summary>
public static class LogAnalyzer
{
    private sealed record Rule(string Id, Severity Severity, Regex Pattern, string Title, string Explanation, string Fix);

    private static Regex R(string p) => new(p, RegexOptions.IgnoreCase | RegexOptions.Compiled | RegexOptions.CultureInvariant);

    private static readonly Rule[] Rules =
    [
        // victory-gpu host.rs fail() / log_removal_context(), draw.rs
        new("device-removed", Severity.Error, R(@"device removed|the host device went|DXGI_ERROR_DEVICE_(REMOVED|HUNG|RESET)"),
            "The graphics card stopped the game (device removed)",
            "The driver reset the GPU while it ran the game's work: a hang, a fault in a translated shader, or a driver timeout.",
            "Try Quality mode or a lower resolution. To find the cause, run the diag build with DRED 'light' and keep the log."),
        // victory-hle xmem.rs commit_limit_message
        new("commit-limit", Severity.Error, R(@"Windows commit limit reached"),
            "Windows ran out of memory to commit",
            "The game asked for memory and Windows refused: RAM plus the paging file is full.",
            "Close other programs, or let Windows manage the paging file size (or make it larger), then start again."),
        // victory-gpu host.rs chosen_adapter
        new("adapter-missing", Severity.Warning, R(@"VICTORY_ADAPTER names no hardware card"),
            "The chosen graphics card was not found",
            "The adapter setting matches no hardware card, so Windows' default card was used instead.",
            "Pick the card again in Settings > Graphics card."),
        // victory-core policy.rs
        new("policy", Severity.Error, R(@"POLICY VIOLATION|refusing to use .* for game-derived data"),
            "A data folder is inside the source tree",
            "The layer never writes caches or saves inside the VIctory source tree (docs\\asset-policy.md).",
            "Leave the cache and data folders empty (the default is %LOCALAPPDATA%\\victory) or choose folders outside it."),
        // victory-loader thunks.rs
        new("unimplemented", Severity.Error, R(@"unimplemented import called"),
            "The game called a console function the layer does not have yet",
            "This build has no implementation for one of the game's imports, so the run stops there.",
            "Nothing to set: it needs a newer build. The log line names the DLL and function."),
        // victory-hle kernel.rs
        new("title-dll", Severity.Error, R(@"mapping a title DLL failed|missing dlls"),
            "A DLL shipped with the game could not be loaded",
            "One of the DLLs next to the game's executable is missing or could not be mapped.",
            "Check that the whole game folder was copied, not just the .exe."),
        new("title-abort", Severity.Error, R(@"title aborts|title calls terminate|title passed an invalid parameter"),
            "The game stopped itself",
            "The game's own code gave up (abort/terminate). Usually something earlier in the log went wrong first.",
            "Look at the warnings just before this line in the log."),
        // victory-loader crash.rs
        new("panic", Severity.Error, R(@"VIctory: panic on thread|panicked at"),
            "The layer itself crashed (a panic)",
            "A check inside VIctory failed. This is a bug in the layer, not in your settings.",
            "Keep the log and crashes.log; they name the place."),
        new("access-violation", Severity.Error, R(@"0xC0000005|access violation"),
            "A memory access fault",
            "Code tried to read or write memory that is not there. The crash report names the module and offset.",
            "Keep crashes.log. If it repeats at the same place, try Quality mode."),
        // apps/victory run.rs (AVX2 check)
        new("avx2", Severity.Error, R(@"does not support AVX2"),
            "This CPU lacks AVX2",
            "The layer needs a CPU with AVX2 instructions.",
            "A CPU with AVX2 is required."),
        new("no-game-dir", Severity.Error, R(@"no game directory configured"),
            "No game folder is set",
            "The layer was started without a game folder.",
            "Choose the game's executable on the Play page."),
        // victory-gpu pipeline/make.rs and pipeline.rs drop counters
        new("pipelines-dropped", Severity.Info, R(@"dropped [\d.]+/frame without a host pipeline"),
            "Draws skipped while new pipelines are built",
            "The first time a place is seen, its pipelines are compiled; draws wait up to the pipeline wait, then are skipped for a moment.",
            "Run 'Build pipeline cache' once after a play session, so the driver has them ready next time."),
    ];

    public static Problem? Match(string line)
    {
        foreach (var r in Rules)
            if (r.Pattern.IsMatch(line))
                return new(r.Id, r.Severity, r.Title, r.Explanation, r.Fix, line.Length > 400 ? line[..400] + "..." : line);
        return null;
    }

    /// <summary>What a process exit code says (the layer's own codes, Rust's, and Windows' crash codes).</summary>
    public static string DescribeExit(int code) => unchecked((uint)code) switch
    {
        0 => "Ended normally.",
        1 => "The layer stopped with an error (see the last lines of the log).",
        2 => "The layer was given arguments it does not know (a launcher/script mismatch).",
        101 => "The layer crashed (a Rust panic).",
        0xC0000005 => "Crashed: access violation (0xC0000005). crashes.log names the place.",
        0xC00000FD => "Crashed: stack overflow (0xC00000FD).",
        0xC0000409 => "Stopped by a fail-fast check (0xC0000409).",
        0xC0000135 => "A DLL it needs was not found (0xC0000135).",
        0xC000007B => "A DLL of the wrong kind (32/64-bit) was loaded (0xC000007B).",
        0xC0000017 or 0xC000012D => "Out of memory (commit limit).",
        0x887A0005 => "The GPU was removed (DXGI_ERROR_DEVICE_REMOVED).",
        0x887A0006 => "The GPU hung (DXGI_ERROR_DEVICE_HUNG).",
        0xC000013A => "Stopped by Ctrl+C or the window was closed.",
        uint.MaxValue => "Stopped by the launcher.",
        var c => $"Ended with code {code} (0x{c:X8}).",
    };
}
