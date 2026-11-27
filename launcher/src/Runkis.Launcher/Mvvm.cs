using System.ComponentModel;
using System.Globalization;
using System.Runtime.CompilerServices;
using System.Windows;
using System.Windows.Data;
using System.Windows.Input;
using System.Windows.Media;
using Runkis.Launcher.Core;

namespace Runkis.Launcher;

public abstract class Observable : INotifyPropertyChanged
{
    public event PropertyChangedEventHandler? PropertyChanged;

    protected bool Set<T>(ref T field, T value, [CallerMemberName] string? name = null)
    {
        if (EqualityComparer<T>.Default.Equals(field, value)) return false;
        field = value;
        Raise(name);
        return true;
    }

    protected void Raise([CallerMemberName] string? name = null) => PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(name));
}

public sealed class Command(Action<object?> run, Func<object?, bool>? can = null) : ICommand
{
    public Command(Action run, Func<bool>? can = null) : this(_ => run(), can == null ? null : _ => can()) { }

    public event EventHandler? CanExecuteChanged
    {
        add => CommandManager.RequerySuggested += value;
        remove => CommandManager.RequerySuggested -= value;
    }

    public bool CanExecute(object? parameter) => can?.Invoke(parameter) ?? true;
    public void Execute(object? parameter) => run(parameter);
}

/// <summary>A figure shown on the dashboard. "—" with a reason when it is not known: never a made-up zero.</summary>
public sealed class Tile(string label) : Observable
{
    private string value = "—", sub = "", why = "Not running";
    private Brush tone = Brushes.Transparent;

    public string Label { get; } = label;
    public string Value { get => value; private set => Set(ref this.value, value); }
    public string Sub { get => sub; private set => Set(ref sub, value); }
    public string Why { get => why; private set => Set(ref why, value); }
    public Brush Tone { get => tone; private set => Set(ref tone, value); }

    public void Show(string v, string s = "", Brush? t = null, string? why = null)
    {
        Value = v; Sub = s; Tone = t ?? Brushes.Transparent; Why = why ?? "";
    }

    public void Unknown(string reason) { Value = "—"; Sub = ""; Tone = Brushes.Transparent; Why = reason; }
}

public sealed class StatusBrush : IValueConverter
{
    public object Convert(object value, Type t, object p, CultureInfo c) => value switch
    {
        CheckStatus.Pass => Application.Current.Resources["Good"],
        CheckStatus.Warning => Application.Current.Resources["Warn"],
        CheckStatus.Fail => Application.Current.Resources["Bad"],
        Severity.Error => Application.Current.Resources["Bad"],
        Severity.Warning => Application.Current.Resources["Warn"],
        _ => Application.Current.Resources["Accent"],
    };

    public object ConvertBack(object value, Type t, object p, CultureInfo c) => throw new NotSupportedException();
}

public sealed class StatusGlyph : IValueConverter
{
    public object Convert(object value, Type t, object p, CultureInfo c) => value switch
    {
        CheckStatus.Pass => "✓",
        CheckStatus.Warning or Severity.Warning => "!",
        CheckStatus.Fail or Severity.Error => "✕",
        _ => "i",
    };

    public object ConvertBack(object value, Type t, object p, CultureInfo c) => throw new NotSupportedException();
}

public sealed class LineBrush : IValueConverter
{
    public object Convert(object value, Type t, object p, CultureInfo c) => value switch
    {
        Severity.Error => Application.Current.Resources["Bad"],
        Severity.Warning => Application.Current.Resources["Warn"],
        _ => Application.Current.Resources["Muted"],
    };

    public object ConvertBack(object value, Type t, object p, CultureInfo c) => throw new NotSupportedException();
}

public sealed class IsPage : IValueConverter
{
    public object Convert(object value, Type t, object p, CultureInfo c) => value is int i && i.ToString(CultureInfo.InvariantCulture) == p as string;
    public object ConvertBack(object value, Type t, object p, CultureInfo c) => Binding.DoNothing;
}

public sealed class VisibleIf : IValueConverter
{
    public object Convert(object value, Type t, object p, CultureInfo c)
    {
        var on = value switch { bool b => b, int i => i > 0, string s => s.Length > 0, null => false, _ => true };
        if (p as string == "not") on = !on;
        return on ? Visibility.Visible : Visibility.Collapsed;
    }

    public object ConvertBack(object value, Type t, object p, CultureInfo c) => throw new NotSupportedException();
}
