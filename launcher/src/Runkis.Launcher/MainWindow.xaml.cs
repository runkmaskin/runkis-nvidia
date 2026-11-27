using System.Globalization;
using System.Windows;
using System.Windows.Data;

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
    }

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
