using System.Globalization;
using System.Runtime.InteropServices;

namespace Runkis.Launcher.Core;

/// <summary>One monitor, numbered from 1 in the order Windows lists them (the order VICTORY_MONITOR counts in).</summary>
public sealed record DisplayMonitor(int Number, int Left, int Top, int Width, int Height, int WorkWidth, int WorkHeight, bool Primary)
{
    public string Label => $"{Number}: {Width} x {Height}{(Primary ? " (main monitor)" : "")}";
}

/// <summary>The monitors, and the display choices that follow from them. Mirrors crates/victory-gpu/src/display.rs.</summary>
public static class Displays
{
    /// <summary>Common window sizes (client area) offered for a framed window.</summary>
    public static readonly (int W, int H)[] CommonSizes = [(1280, 720), (1600, 900), (1920, 1080), (2560, 1440), (3840, 2160)];

    /// <summary>"WIDTHxHEIGHT", both 1..16384, as the layer reads VICTORY_WINDOW_SIZE.</summary>
    public static bool TryParseSize(string? text, out int width, out int height)
    {
        width = height = 0;
        if (string.IsNullOrWhiteSpace(text)) return false;
        var parts = text.Trim().Split('x', 'X');
        return parts.Length == 2
               && int.TryParse(parts[0].Trim(), NumberStyles.None, CultureInfo.InvariantCulture, out width)
               && int.TryParse(parts[1].Trim(), NumberStyles.None, CultureInfo.InvariantCulture, out height)
               && width is >= 1 and <= 16384 && height is >= 1 and <= 16384;
    }

    public static string Size(int w, int h) => $"{w.ToString(CultureInfo.InvariantCulture)}x{h.ToString(CultureInfo.InvariantCulture)}";

    /// <summary>Monitor <paramref name="number"/>, or the main one when it is 0 or not connected (as the layer picks).</summary>
    public static DisplayMonitor? Pick(IReadOnlyList<DisplayMonitor> monitors, int number) =>
        (number >= 1 && number <= monitors.Count ? monitors[number - 1] : null)
        ?? monitors.FirstOrDefault(m => m.Primary) ?? monitors.FirstOrDefault();

    /// <summary>The window sizes to offer: the common ones smaller than the monitor, then the monitor's own size.</summary>
    public static List<string> WindowSizes(DisplayMonitor? monitor)
    {
        var sizes = CommonSizes.Where(s => monitor == null || s.W < monitor.Width && s.H < monitor.Height).Select(s => Size(s.W, s.H)).ToList();
        if (monitor != null && !sizes.Contains(Size(monitor.Width, monitor.Height))) sizes.Add(Size(monitor.Width, monitor.Height));
        return sizes;
    }

    /// <summary>
    /// The sharp upscale is worth having by default when the window covers a monitor above 1080p: the game draws
    /// 1080p and Windows' own stretch is blurry.
    /// </summary>
    public static bool SuggestPresentScale(WindowMode mode, DisplayMonitor? monitor) =>
        mode != WindowMode.Windowed && monitor is { Height: > 1080 };

    /// <summary>The monitors connected now, in Windows' order.</summary>
    public static List<DisplayMonitor> List()
    {
        var found = new List<DisplayMonitor>();
        try
        {
            EnumDisplayMonitors(IntPtr.Zero, IntPtr.Zero, (monitor, _, _, _) =>
            {
                var info = new MonitorInfo { Size = Marshal.SizeOf<MonitorInfo>() };
                if (GetMonitorInfoW(monitor, ref info))
                    found.Add(new DisplayMonitor(found.Count + 1, info.Monitor.Left, info.Monitor.Top,
                        info.Monitor.Right - info.Monitor.Left, info.Monitor.Bottom - info.Monitor.Top,
                        info.Work.Right - info.Work.Left, info.Work.Bottom - info.Work.Top, (info.Flags & 1) != 0));
                return true;
            }, IntPtr.Zero);
        }
        catch (Exception e) when (e is DllNotFoundException or EntryPointNotFoundException) { }
        return found;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct Rect { public int Left, Top, Right, Bottom; }

    [StructLayout(LayoutKind.Sequential)]
    private struct MonitorInfo { public int Size; public Rect Monitor; public Rect Work; public uint Flags; }

    private delegate bool MonitorEnumProc(IntPtr monitor, IntPtr hdc, IntPtr rect, IntPtr data);

    [DllImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool EnumDisplayMonitors(IntPtr hdc, IntPtr clip, MonitorEnumProc callback, IntPtr data);

    [DllImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool GetMonitorInfoW(IntPtr monitor, ref MonitorInfo info);
}
