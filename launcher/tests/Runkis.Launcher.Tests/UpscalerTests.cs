using Runkis.Launcher.Core;
using Xunit;

namespace Runkis.Launcher.Tests;

public class UpscalerTests
{
    [Fact]
    public void Each_upscaler_sets_exactly_its_switches()
    {
        var game = PlanBuilder.Overrides(new LaunchProfile { Upscaler = UpscalerMode.Game });
        Assert.False(game.ContainsKey("VICTORY_OWN_TAAU"));
        Assert.False(game.ContainsKey("VICTORY_DLSS"));

        var taau = PlanBuilder.Overrides(new LaunchProfile { Upscaler = UpscalerMode.GameTemporal });
        Assert.Equal("1", taau["VICTORY_OWN_TAAU"]);
        Assert.False(taau.ContainsKey("VICTORY_DLSS"));

        var dlss = PlanBuilder.Overrides(new LaunchProfile { Upscaler = UpscalerMode.Dlss });
        Assert.Equal("1", dlss["VICTORY_OWN_TAAU"]);
        Assert.Equal("1", dlss["VICTORY_DLSS"]);
        Assert.Equal("1", dlss["VICTORY_DLSS_REFLEX"]);
    }

    [Fact]
    public void Presets_keep_the_upscaler()
    {
        var p = Presets.Make(Presets.All[0], new LaunchProfile { GameExe = "x.exe", Upscaler = UpscalerMode.Dlss });
        Assert.Equal(UpscalerMode.Dlss, p.Upscaler);
    }
}