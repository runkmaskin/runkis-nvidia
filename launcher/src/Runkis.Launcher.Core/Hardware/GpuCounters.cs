using System.Diagnostics;

namespace Runkis.Launcher.Core;

/// <summary>GPU use as Windows counts it (the figures Task Manager shows). Null: the counter is not there.</summary>
public sealed record GpuUse(double? AdapterBusy, double? Process3D, double? ProcessCompute, double? ProcessCopy, ulong? ProcessDedicated, ulong? AdapterDedicated);

/// <summary>
/// Reads the "GPU Engine", "GPU Process Memory" and "GPU Adapter Memory" counter sets once a call, all instances
/// at a time. Engine utilisation is a rate, so the first call returns nulls for it.
/// </summary>
public sealed class GpuCounters
{
    private Dictionary<string, CounterSample> previous = [];
    public string? Error { get; private set; }

    public GpuUse Sample(int? pid, string? luid)
    {
        double? busy = null, p3d = null, pcompute = null, pcopy = null;
        ulong? pmem = null, amem = null;
        try
        {
            var engines = new PerformanceCounterCategory("GPU Engine").ReadCategory()["Utilization Percentage"];
            if (engines != null)
            {
                var now = new Dictionary<string, CounterSample>();
                var perEngine = new Dictionary<string, double>();   // per adapter engine, summed over processes
                double s3d = 0, scompute = 0, scopy = 0;
                var any = false;
                foreach (InstanceData d in engines.Values)
                {
                    now[d.InstanceName] = d.Sample;
                    if (!previous.TryGetValue(d.InstanceName, out var before)) continue;
                    if (luid != null && !d.InstanceName.Contains(luid, StringComparison.OrdinalIgnoreCase)) continue;
                    var v = CounterSample.Calculate(before, d.Sample);
                    any = true;
                    // "pid_1234_luid_0x..._phys_0_eng_3_engtype_3D"
                    var engAt = d.InstanceName.IndexOf("_phys_", StringComparison.Ordinal);
                    var engine = engAt >= 0 ? d.InstanceName[engAt..] : d.InstanceName;
                    perEngine[engine] = perEngine.GetValueOrDefault(engine) + v;
                    if (pid is int id && d.InstanceName.StartsWith($"pid_{id}_", StringComparison.Ordinal))
                    {
                        if (d.InstanceName.EndsWith("engtype_3D", StringComparison.OrdinalIgnoreCase)) s3d += v;
                        else if (d.InstanceName.Contains("engtype_Compute", StringComparison.OrdinalIgnoreCase)) scompute += v;
                        else if (d.InstanceName.Contains("engtype_Copy", StringComparison.OrdinalIgnoreCase)) scopy += v;
                    }
                }
                previous = now;
                if (any)
                {
                    // The busiest engine is how busy the card is (as Task Manager reports it).
                    busy = Math.Min(100, perEngine.Count > 0 ? perEngine.Values.Max() : 0);
                    if (pid != null) { p3d = Math.Min(100, s3d); pcompute = Math.Min(100, scompute); pcopy = Math.Min(100, scopy); }
                }
            }
            if (pid is int procId)
            {
                var mem = new PerformanceCounterCategory("GPU Process Memory").ReadCategory()["Dedicated Usage"];
                if (mem != null)
                {
                    ulong total = 0;
                    var found = false;
                    foreach (InstanceData d in mem.Values)
                        if (d.InstanceName.StartsWith($"pid_{procId}_", StringComparison.Ordinal)
                            && (luid == null || d.InstanceName.Contains(luid, StringComparison.OrdinalIgnoreCase)))
                        { total += (ulong)Math.Max(0, d.RawValue); found = true; }
                    if (found) pmem = total;
                }
            }
            var adapter = new PerformanceCounterCategory("GPU Adapter Memory").ReadCategory()["Dedicated Usage"];
            if (adapter != null && luid != null)
                foreach (InstanceData d in adapter.Values)
                    if (d.InstanceName.Contains(luid, StringComparison.OrdinalIgnoreCase)) amem = (amem ?? 0) + (ulong)Math.Max(0, d.RawValue);
            Error = null;
        }
        catch (Exception e) when (e is InvalidOperationException or UnauthorizedAccessException or System.ComponentModel.Win32Exception)
        {
            Error = $"GPU counters not readable: {e.Message}";
        }
        return new GpuUse(busy, p3d, pcompute, pcopy, pmem, amem);
    }
}
