using System.Runtime.InteropServices;
using System.Text;

namespace Runkis.Launcher.Core;

/// <summary>Live figures of an NVIDIA card. A null field was not reported by the driver; it is never a stand-in zero.</summary>
public sealed record NvidiaReading(
    string Name, uint? GpuUtil, uint? MemUtil, ulong? MemUsed, ulong? MemTotal, uint? TemperatureC,
    uint? GraphicsClockMhz, uint? MemoryClockMhz, double? PowerW, double? PowerLimitW);

/// <summary>
/// NVIDIA's management library (nvml.dll, installed with the driver), read in this process: no nvidia-smi child
/// processes. Absent on other cards and on old drivers; then <see cref="Available"/> is false.
/// </summary>
public sealed unsafe class Nvml : IDisposable
{
    private readonly IntPtr lib;
    private readonly delegate* unmanaged[Cdecl]<int> shutdown;
    private readonly delegate* unmanaged[Cdecl]<uint*, int> count;
    private readonly delegate* unmanaged[Cdecl]<uint, IntPtr*, int> handle;
    private readonly delegate* unmanaged[Cdecl]<IntPtr, byte*, uint, int> name;
    private readonly delegate* unmanaged[Cdecl]<IntPtr, uint*, int> utilization;   // nvmlUtilization_t {gpu, memory}
    private readonly delegate* unmanaged[Cdecl]<IntPtr, ulong*, int> memory;       // nvmlMemory_t {total, free, used}
    private readonly delegate* unmanaged[Cdecl]<IntPtr, int, uint*, int> temperature;
    private readonly delegate* unmanaged[Cdecl]<IntPtr, int, uint*, int> clock;
    private readonly delegate* unmanaged[Cdecl]<IntPtr, uint*, int> power;
    private readonly delegate* unmanaged[Cdecl]<IntPtr, uint*, int> powerLimit;
    private readonly delegate* unmanaged[Cdecl]<byte*, uint, int> driverVersion;

    public bool Available { get; }
    public string? DriverVersion { get; }
    public string? Error { get; }

    public Nvml()
    {
        var path = Path.Combine(Environment.SystemDirectory, "nvml.dll");
        if (!NativeLibrary.TryLoad(path, out lib) && !NativeLibrary.TryLoad(
                Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ProgramFiles), "NVIDIA Corporation", "NVSMI", "nvml.dll"), out lib))
        {
            Error = "nvml.dll not found (no NVIDIA driver)";
            return;
        }
        IntPtr F(string n) => NativeLibrary.TryGetExport(lib, n, out var f) ? f : IntPtr.Zero;
        var init = (delegate* unmanaged[Cdecl]<int>)F("nvmlInit_v2");
        shutdown = (delegate* unmanaged[Cdecl]<int>)F("nvmlShutdown");
        count = (delegate* unmanaged[Cdecl]<uint*, int>)F("nvmlDeviceGetCount_v2");
        handle = (delegate* unmanaged[Cdecl]<uint, IntPtr*, int>)F("nvmlDeviceGetHandleByIndex_v2");
        name = (delegate* unmanaged[Cdecl]<IntPtr, byte*, uint, int>)F("nvmlDeviceGetName");
        utilization = (delegate* unmanaged[Cdecl]<IntPtr, uint*, int>)F("nvmlDeviceGetUtilizationRates");
        memory = (delegate* unmanaged[Cdecl]<IntPtr, ulong*, int>)F("nvmlDeviceGetMemoryInfo");
        temperature = (delegate* unmanaged[Cdecl]<IntPtr, int, uint*, int>)F("nvmlDeviceGetTemperature");
        clock = (delegate* unmanaged[Cdecl]<IntPtr, int, uint*, int>)F("nvmlDeviceGetClockInfo");
        power = (delegate* unmanaged[Cdecl]<IntPtr, uint*, int>)F("nvmlDeviceGetPowerUsage");
        powerLimit = (delegate* unmanaged[Cdecl]<IntPtr, uint*, int>)F("nvmlDeviceGetEnforcedPowerLimit");
        driverVersion = (delegate* unmanaged[Cdecl]<byte*, uint, int>)F("nvmlSystemGetDriverVersion");
        if (init == null || count == null || handle == null || name == null)
        {
            Error = "nvml.dll lacks the functions needed";
            return;
        }
        var rc = init();
        if (rc != 0)
        {
            Error = $"NVML did not start (code {rc})";
            return;
        }
        Available = true;
        if (driverVersion != null)
        {
            var buf = stackalloc byte[96];
            if (driverVersion(buf, 96) == 0) DriverVersion = Str(buf);
        }
    }

    private static string Str(byte* b) => Encoding.ASCII.GetString(b, new ReadOnlySpan<byte>(b, 96).IndexOf((byte)0) is var n and >= 0 ? n : 96);

    public List<NvidiaReading> Read()
    {
        var list = new List<NvidiaReading>();
        if (!Available) return list;
        uint n;
        if (count(&n) != 0) return list;
        var nb = stackalloc byte[96];
        uint* u = stackalloc uint[2];
        ulong* m = stackalloc ulong[3];
        for (uint i = 0; i < n; i++)
        {
            IntPtr dev;
            if (handle(i, &dev) != 0) continue;
            var devName = name(dev, nb, 96) == 0 ? Str(nb) : $"NVIDIA #{i}";
            var hasUtil = utilization != null && utilization(dev, u) == 0;
            var hasMem = memory != null && memory(dev, m) == 0;
            uint t = 0, gc = 0, mc = 0, pw = 0, pl = 0;
            var hasT = temperature != null && temperature(dev, 0, &t) == 0;
            var hasGc = clock != null && clock(dev, 0, &gc) == 0;   // NVML_CLOCK_GRAPHICS
            var hasMc = clock != null && clock(dev, 2, &mc) == 0;   // NVML_CLOCK_MEM
            var hasP = power != null && power(dev, &pw) == 0;
            var hasPl = powerLimit != null && powerLimit(dev, &pl) == 0;
            list.Add(new NvidiaReading(devName,
                hasUtil ? u[0] : null, hasUtil ? u[1] : null,
                hasMem ? m[2] : null, hasMem ? m[0] : null,
                hasT ? t : null, hasGc ? gc : null, hasMc ? mc : null,
                hasP ? pw / 1000.0 : null, hasPl ? pl / 1000.0 : null));
        }
        return list;
    }

    public void Dispose()
    {
        if (Available && shutdown != null) shutdown();
        if (lib != IntPtr.Zero) NativeLibrary.Free(lib);
    }
}
