using System.Runtime.InteropServices;
using System.Security.Principal;
using Microsoft.Win32;

namespace Runkis.Launcher.Core;

/// <summary>What the PC is; read once (cheap) and again on demand.</summary>
public sealed record SystemSnapshot(
    string CpuName, int PhysicalCores, int LogicalProcessors, ulong TotalRam, ulong AvailableRam,
    ulong CommitLimit, ulong CommitAvailable, string Os, bool? HardwareScheduling, string? PowerPlan, bool IsAdmin);

public static class SystemInfo
{
    public static SystemSnapshot Read()
    {
        var mem = Native.Memory();
        return new SystemSnapshot(
            CpuName(), Native.PhysicalCores(), Environment.ProcessorCount,
            mem.TotalPhys, mem.AvailPhys, mem.TotalPageFile, mem.AvailPageFile,
            RuntimeInformation.OSDescription, HardwareScheduling(), PowerPlan(), IsAdmin());
    }

    /// <summary>Physical RAM: total and available bytes, now.</summary>
    public static (ulong Total, ulong Avail) MemoryNow()
    {
        var m = Native.Memory();
        return (m.TotalPhys, m.AvailPhys);
    }

    private static string CpuName()
    {
        using var k = Registry.LocalMachine.OpenSubKey(@"HARDWARE\DESCRIPTION\System\CentralProcessor\0");
        return (k?.GetValue("ProcessorNameString") as string)?.Trim() ?? "Unknown CPU";
    }

    /// <summary>Hardware-accelerated GPU scheduling (HwSchMode 2 = on, 1 = off; absent = not offered).</summary>
    private static bool? HardwareScheduling()
    {
        using var k = Registry.LocalMachine.OpenSubKey(@"SYSTEM\CurrentControlSet\Control\GraphicsDrivers");
        return k?.GetValue("HwSchMode") is int v ? v == 2 : null;
    }

    [DllImport("powrprof.dll")]
    private static extern uint PowerGetActiveScheme(IntPtr root, out IntPtr scheme);

    [DllImport("powrprof.dll", CharSet = CharSet.Unicode)]
    private static extern uint PowerReadFriendlyName(IntPtr root, IntPtr scheme, IntPtr sub, IntPtr setting, IntPtr buffer, ref uint size);

    [DllImport("kernel32.dll")]
    private static extern IntPtr LocalFree(IntPtr mem);

    public static readonly Guid HighPerformance = new("8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c");
    public static readonly Guid Ultimate = new("e9a42b02-d5df-448d-aa00-03f14749eb61");

    public static Guid? PowerPlanGuid()
    {
        if (PowerGetActiveScheme(IntPtr.Zero, out var g) != 0) return null;
        try { return Marshal.PtrToStructure<Guid>(g); }
        finally { LocalFree(g); }
    }

    private static string? PowerPlan()
    {
        if (PowerGetActiveScheme(IntPtr.Zero, out var g) != 0) return null;
        try
        {
            uint size = 0;
            PowerReadFriendlyName(IntPtr.Zero, g, IntPtr.Zero, IntPtr.Zero, IntPtr.Zero, ref size);
            if (size == 0) return null;
            var buf = Marshal.AllocHGlobal((int)size);
            try
            {
                return PowerReadFriendlyName(IntPtr.Zero, g, IntPtr.Zero, IntPtr.Zero, buf, ref size) == 0 ? Marshal.PtrToStringUni(buf) : null;
            }
            finally { Marshal.FreeHGlobal(buf); }
        }
        finally { LocalFree(g); }
    }

    private static bool IsAdmin()
    {
        using var id = WindowsIdentity.GetCurrent();
        return new WindowsPrincipal(id).IsInRole(WindowsBuiltInRole.Administrator);
    }
}

/// <summary>Whole-system CPU use from GetSystemTimes deltas (null on the first call).</summary>
public sealed class CpuMeter
{
    private long idle, kernel, user;
    private bool primed;

    public double? Sample()
    {
        if (!Native.GetSystemTimes(out var i, out var k, out var u)) return null;
        double? result = null;
        if (primed)
        {
            var total = (k - kernel) + (u - user);   // kernel time includes idle time
            if (total > 0) result = Math.Clamp(100.0 * (total - (i - idle)) / total, 0, 100);
        }
        (idle, kernel, user, primed) = (i, k, u, true);
        return result;
    }
}
