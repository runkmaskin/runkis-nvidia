using System.IO;
using System.Windows;
using System.Windows.Threading;
using Runkis.Launcher.Core;

namespace Runkis.Launcher;

public partial class App : Application
{
    protected override void OnStartup(StartupEventArgs e)
    {
        // An unexpected error is shown and written down instead of closing the launcher silently.
        DispatcherUnhandledException += (_, args) =>
        {
            Report(args.Exception);
            args.Handled = true;
        };
        base.OnStartup(e);
    }

    private static void Report(Exception e)
    {
        try
        {
            Directory.CreateDirectory(AppPaths.Logs);
            File.AppendAllText(Path.Combine(AppPaths.Logs, "launcher-errors.txt"), $"{DateTime.Now:O}\n{e}\n\n");
        }
        catch (IOException) { }
        MessageBox.Show($"Something went wrong in the launcher:\n\n{e.Message}\n\nDetails were written to {AppPaths.Logs}\\launcher-errors.txt",
            "Runkis Launcher", MessageBoxButton.OK, MessageBoxImage.Warning);
    }
}
