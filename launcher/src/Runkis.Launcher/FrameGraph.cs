using System.Globalization;
using System.Windows;
using System.Windows.Media;

namespace Runkis.Launcher;

/// <summary>Frame times of the last frames as a line, with the 30/20/10 fps marks. Drawn in one pass, no per-point elements.</summary>
public sealed class FrameGraph : FrameworkElement
{
    public static readonly DependencyProperty SamplesProperty = DependencyProperty.Register(
        nameof(Samples), typeof(double[]), typeof(FrameGraph),
        new FrameworkPropertyMetadata(Array.Empty<double>(), FrameworkPropertyMetadataOptions.AffectsRender));

    public double[] Samples
    {
        get => (double[])GetValue(SamplesProperty);
        set => SetValue(SamplesProperty, value);
    }

    private static readonly Pen Grid = Freeze(new Pen(new SolidColorBrush(Color.FromRgb(0x2A, 0x33, 0x41)), 1) { DashStyle = DashStyles.Dash });
    private static readonly Pen Line = Freeze(new Pen(new SolidColorBrush(Color.FromRgb(0x4F, 0x8C, 0xFF)), 1.4));
    private static readonly Brush Fill = Freeze(new SolidColorBrush(Color.FromArgb(0x30, 0x4F, 0x8C, 0xFF)));
    private static readonly Brush Spike = Freeze(new SolidColorBrush(Color.FromRgb(0xF2, 0x61, 0x5C)));
    private static readonly Brush LabelBrush = Freeze(new SolidColorBrush(Color.FromRgb(0x5D, 0x68, 0x78)));
    private static readonly Typeface Face = new("Segoe UI");

    private static T Freeze<T>(T f) where T : Freezable { f.Freeze(); return f; }

    protected override void OnRender(DrawingContext dc)
    {
        double w = ActualWidth, h = ActualHeight;
        if (w < 10 || h < 10) return;
        var s = Samples;
        // Scale to the slowest frame shown, at least 120 ms so 30 fps sits in the lower part, at most 400.
        var top = Math.Clamp(s.Length > 0 ? s.Max() * 1.1 : 120, 120, 400);
        var dpi = VisualTreeHelper.GetDpi(this).PixelsPerDip;
        foreach (var (ms, label) in new[] { (33.3, "30 fps"), (50.0, "20 fps"), (100.0, "10 fps") })
        {
            var y = h - ms / top * h;
            dc.DrawLine(Grid, new Point(0, y), new Point(w, y));
            dc.DrawText(new FormattedText(label, CultureInfo.InvariantCulture, FlowDirection.LeftToRight, Face, 10, LabelBrush, dpi), new Point(4, y - 14));
        }
        if (s.Length < 2) return;
        var step = w / (s.Length - 1);
        var geo = new StreamGeometry();
        using (var g = geo.Open())
        {
            g.BeginFigure(new Point(0, h), true, true);
            for (var i = 0; i < s.Length; i++) g.LineTo(new Point(i * step, h - Math.Min(s[i], top) / top * h), true, true);
            g.LineTo(new Point(w, h), false, false);
        }
        geo.Freeze();
        dc.DrawGeometry(Fill, null, geo);
        var line = new StreamGeometry();
        using (var g = line.Open())
        {
            g.BeginFigure(new Point(0, h - Math.Min(s[0], top) / top * h), false, false);
            for (var i = 1; i < s.Length; i++) g.LineTo(new Point(i * step, h - Math.Min(s[i], top) / top * h), true, true);
        }
        line.Freeze();
        dc.DrawGeometry(null, Line, line);
        // Hitches (100 ms or more) marked so they stand out.
        for (var i = 0; i < s.Length; i++)
            if (s[i] >= 100) dc.DrawEllipse(Spike, null, new Point(i * step, h - Math.Min(s[i], top) / top * h), 2.5, 2.5);
    }
}
