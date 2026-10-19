using Microsoft.UI.Xaml;

namespace Qh;

partial class App : Application
{
    MainWindow? window;
    static readonly string CrashLog = Path.Combine(Path.GetTempPath(), "qh-app-crash.txt");

    public App()
    {
        InitializeComponent();
        UnhandledException += (_, e) => { File.AppendAllText(CrashLog, $"{DateTime.Now:s} {e.Exception}\n"); e.Handled = true; };
    }

    protected override void OnLaunched(LaunchActivatedEventArgs args)
    {
        try
        {
            window = new MainWindow();
            window.Activate();
        }
        catch (Exception e) { File.AppendAllText(CrashLog, $"{DateTime.Now:s} launch: {e}\n"); throw; }
    }
}
