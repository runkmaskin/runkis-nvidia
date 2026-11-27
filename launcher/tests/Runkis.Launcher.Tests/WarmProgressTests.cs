using Runkis.Launcher.Core;
using Xunit;

namespace Runkis.Launcher.Tests;

public class WarmProgressTests
{
    [Fact]
    public void Pipelines_take_the_total_from_the_first_line_and_the_count_from_the_steps()
    {
        var w = new WarmProgress();
        Assert.Null(w.Percent);
        Assert.True(w.Take("83564 pipelines known, 0 made before (95d12808af53e887), 0 bad; 83564 to make, 10 processes"));
        Assert.Equal(83564, w.Total);
        Assert.Equal(0.0, w.Percent);
        Assert.True(w.Take("19000 of 83564, 474 s"));
        Assert.Equal(19000, w.Done);
        Assert.InRange(w.Percent!.Value, 22.7, 22.8);
        Assert.False(w.Take("2026-11-27T13:53:37.265394Z  INFO victory_gpu::host: host wave lanes min=32 max=32"));
    }

    [Fact]
    public void Time_left_follows_the_last_five_minutes_not_the_whole_run()
    {
        var w = new WarmProgress();
        w.Take("10000 pipelines known, 0 made before (x), 0 bad; 10000 to make, 10 processes");
        w.Take("4000 of 10000, 60 s");     // fast at first
        Assert.Null(w.Left);               // one sample: no rate yet
        w.Take("5000 of 10000, 500 s");
        w.Take("6000 of 10000, 700 s");    // the last 5 minutes: 1000 in 200 s = 5 a second
        Assert.Equal(800, w.Left!.Value.TotalSeconds, 3);
        Assert.Equal("6 000 of 10 000 pipelines (60 %) · about 14 min left", w.Describe("pipelines"));
    }

    [Fact]
    public void Shaders_use_the_last_total_and_learn_the_new_one_at_the_end()
    {
        var w = new WarmProgress(knownTotal: 40000);
        Assert.True(w.Take("10000 shaders in the cache, 80 s"));
        Assert.Equal(25.0, w.Percent);
        Assert.True(w.Take("39990 shaders in the cache, 7 do not translate, 310 s"));
        Assert.Equal(39997, w.FinishedTotal);
        Assert.Equal(100.0, w.Percent);
    }

    [Fact]
    public void Shaders_without_a_known_total_show_the_count_alone()
    {
        var w = new WarmProgress();
        w.Take("1250 shaders in the cache, 9 s");
        Assert.Null(w.Percent);
        Assert.Equal("1 250 shaders", w.Describe("shaders"));
        // More than the last translation had: another game build, so the old total is dropped.
        var old = new WarmProgress(knownTotal: 1000);
        old.Take("1250 shaders in the cache, 9 s");
        Assert.Null(old.Percent);
    }
}
