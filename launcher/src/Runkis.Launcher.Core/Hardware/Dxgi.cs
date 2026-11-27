using System.Runtime.InteropServices;

namespace Runkis.Launcher.Core;

/// <summary>A graphics adapter as DXGI lists it: the same list, in the same order, the layer picks from.</summary>
public sealed record GpuAdapter(
    int Index, string Name, uint VendorId, uint DeviceId, ulong DedicatedVideoMemory, ulong SharedSystemMemory,
    long Luid, bool Software, string? DriverVersion)
{
    public bool IsNvidia => VendorId == 0x10DE;
    public bool IsAmd => VendorId == 0x1002;
    public bool IsIntel => VendorId == 0x8086;
    public string Vendor => VendorId switch { 0x10DE => "NVIDIA", 0x1002 => "AMD", 0x8086 => "Intel", 0x1414 => "Microsoft", _ => $"0x{VendorId:X4}" };
    /// <summary>"luid_0x{high}_0x{low}" as Windows' GPU performance counters name the adapter.</summary>
    public string CounterLuid => $"luid_0x{(uint)(Luid >> 32):X8}_0x{(uint)Luid:X8}";

    /// <summary>NVIDIA's own version number from Windows' (32.0.16.2114 -> 621.14).</summary>
    public string? NvidiaVersion
    {
        get
        {
            if (!IsNvidia || DriverVersion == null) return null;
            var digits = DriverVersion.Replace(".", "");
            if (digits.Length < 5) return null;
            var last = digits[^5..];
            return $"{int.Parse(last[..3])}.{last[3..]}";
        }
    }
}

public static class Dxgi
{
    private static readonly Guid IidFactory1 = new("770aae78-f26f-4dba-a829-253c83d1b387");
    private static readonly Guid IidDxgiDevice = new("54ec77fa-1377-44e6-8c32-88fd5f44c84c");

    [DllImport("dxgi.dll")]
    private static extern int CreateDXGIFactory1(in Guid riid, out IntPtr factory);

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    private struct AdapterDesc1
    {
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string Description;
        public uint VendorId, DeviceId, SubSysId, Revision;
        public nuint DedicatedVideoMemory, DedicatedSystemMemory, SharedSystemMemory;
        public uint LuidLow;
        public int LuidHigh;
        public uint Flags;
    }

    // Raw vtable calls keep this free of COM interop declarations: IDXGIFactory1::EnumAdapters1 is slot 12,
    // IDXGIAdapter::CheckInterfaceSupport slot 9, IDXGIAdapter1::GetDesc1 slot 10, IUnknown::Release slot 2.
    private static unsafe IntPtr Slot(IntPtr obj, int i) => (*(IntPtr**)obj)[i];

    private static unsafe void Release(IntPtr obj)
    {
        if (obj != IntPtr.Zero) ((delegate* unmanaged[Stdcall]<IntPtr, uint>)Slot(obj, 2))(obj);
    }

    /// <summary>Every adapter, hardware and software, in DXGI's order (#0 is the one the main display is on).</summary>
    public static unsafe List<GpuAdapter> Adapters()
    {
        var list = new List<GpuAdapter>();
        if (CreateDXGIFactory1(IidFactory1, out var factory) < 0 || factory == IntPtr.Zero) return list;
        try
        {
            var enumAdapters1 = (delegate* unmanaged[Stdcall]<IntPtr, uint, IntPtr*, int>)Slot(factory, 12);
            for (uint i = 0; ; i++)
            {
                IntPtr adapter;
                if (enumAdapters1(factory, i, &adapter) < 0) break;
                try
                {
                    var getDesc1 = (delegate* unmanaged[Stdcall]<IntPtr, IntPtr, int>)Slot(adapter, 10);
                    var size = Marshal.SizeOf<AdapterDesc1>();
                    var buf = Marshal.AllocHGlobal(size);
                    AdapterDesc1 d;
                    try
                    {
                        if (getDesc1(adapter, buf) < 0) continue;
                        d = Marshal.PtrToStructure<AdapterDesc1>(buf);
                    }
                    finally { Marshal.FreeHGlobal(buf); }
                    // The user-mode driver's version, the same query the layer logs (victory-gpu host.rs Host::driver).
                    string? version = null;
                    var check = (delegate* unmanaged[Stdcall]<IntPtr, Guid*, long*, int>)Slot(adapter, 9);
                    long umd;
                    var iid = IidDxgiDevice;
                    if (check(adapter, &iid, &umd) >= 0)
                        version = $"{(umd >> 48) & 0xFFFF}.{(umd >> 32) & 0xFFFF}.{(umd >> 16) & 0xFFFF}.{umd & 0xFFFF}";
                    list.Add(new GpuAdapter((int)i, d.Description.Trim(), d.VendorId, d.DeviceId,
                        d.DedicatedVideoMemory, d.SharedSystemMemory,
                        ((long)d.LuidHigh << 32) | d.LuidLow, (d.Flags & 2) != 0, version));
                }
                finally { Release(adapter); }
            }
        }
        finally { Release(factory); }
        return list;
    }

    /// <summary>
    /// The adapter the layer will use for an adapter setting, by the same rule as victory-gpu host.rs chosen_adapter:
    /// "#n" is DXGI index n, anything else is part of the name in any case; software adapters never match; no match
    /// (or no setting) means Windows' default, which is #0.
    /// </summary>
    public static (GpuAdapter? Adapter, bool Matched) Resolve(IReadOnlyList<GpuAdapter> adapters, string? setting)
    {
        var want = setting?.Trim() ?? "";
        var fallback = adapters.FirstOrDefault(a => a.Index == 0);
        if (want.Length == 0) return (fallback, true);
        int? index = want.StartsWith('#') && int.TryParse(want[1..], out var n) ? n : null;
        foreach (var a in adapters)
        {
            var hit = index is int k ? a.Index == k : a.Name.Contains(want, StringComparison.OrdinalIgnoreCase);
            if (hit && !a.Software) return (a, true);
        }
        return (fallback, false);
    }
}
