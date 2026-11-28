using System.Globalization;
using System.Windows;
using System.Windows.Data;
using System.Windows.Input;

namespace Runkis.Launcher;

public partial class MainWindow : Window
{
    private readonly MainViewModel vm;

    public MainWindow()
    {
        InitializeComponent();
        vm = new MainViewModel();
        DataContext = vm;
        vm.LogAdded += () =>
        {
            if (LogList.Items.Count > 0) LogList.ScrollIntoView(LogList.Items[^1]);
        };
        Closing += (_, e) => { if (!vm.ConfirmClose()) e.Cancel = true; };
        Closed += (_, _) => vm.Dispose();
        PreviewKeyDown += CaptureKey;
        PreviewMouseDown += CaptureMouse;
    }

    /// <summary>While a Controls row waits for a key, the next key goes to it and nowhere else.</summary>
    private void CaptureKey(object sender, KeyEventArgs e)
    {
        if (!vm.Controls.IsCapturing) return;
        e.Handled = true;
        // Alt and F10 arrive as system keys.
        var key = e.Key == Key.System ? e.SystemKey : e.Key;
        if (key == Key.Escape) { vm.Controls.CancelCapture(); return; }
        if (KeyName(key) is { } name) vm.Controls.Captured(name);
        else Toast($"{key} is not a key the emulator reads; press another (Esc cancels).");
    }

    private void CaptureMouse(object sender, MouseButtonEventArgs e)
    {
        if (!vm.Controls.IsCapturing) return;
        e.Handled = true;
        vm.Controls.Captured(e.ChangedButton switch
        {
            MouseButton.Left => "Mouse1",
            MouseButton.Right => "Mouse2",
            MouseButton.Middle => "Mouse3",
            MouseButton.XButton1 => "Mouse4",
            _ => "Mouse5",
        });
    }

    private void Toast(string text) => vm.ShowToast(text);

    /// <summary>A WPF key as the layer names it (gameinput.rs, KEY_NAMES), or null for keys it does not read.</summary>
    internal static string? KeyName(Key key) => key switch
    {
        >= Key.A and <= Key.Z => key.ToString(),
        >= Key.D0 and <= Key.D9 => ((char)('0' + (key - Key.D0))).ToString(),
        >= Key.F1 and <= Key.F12 => key.ToString(),
        Key.Space => "Space",
        Key.Enter => "Enter",
        Key.Tab => "Tab",
        Key.Back => "Back",
        // The layer reads the left modifier keys only.
        Key.LeftShift => "Shift",
        Key.LeftCtrl => "Ctrl",
        Key.LeftAlt => "Alt",
        Key.Up => "Up",
        Key.Down => "Down",
        Key.Left => "Left",
        Key.Right => "Right",
        _ => null,
    };

    private void Minimize(object sender, RoutedEventArgs e) => WindowState = WindowState.Minimized;
    private void Maximize(object sender, RoutedEventArgs e) => WindowState = WindowState == WindowState.Maximized ? WindowState.Normal : WindowState.Maximized;
    private void CloseWindow(object sender, RoutedEventArgs e) => Close();
}

/// <summary>Small display converters for the System page.</summary>
public sealed class Bytes : IValueConverter
{
    private readonly Func<object?, string> f;
    private Bytes(Func<object?, string> f) => this.f = f;

    public static Bytes GB { get; } = new(v => v is ulong b ? $"{b / 1073741824.0:0.#} GB" : "—");
    public static Bytes OnOff { get; } = new(v => v is bool b ? (b ? "on" : "off") : "not offered");

    public object Convert(object? value, Type t, object? p, CultureInfo c) => f(value);
    public object ConvertBack(object? value, Type t, object? p, CultureInfo c) => throw new NotSupportedException();
}
