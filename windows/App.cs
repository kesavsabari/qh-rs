using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace Qh;

/// Code-only WinUI app: the Fluent control styles come from XamlControlsResources, the rest is ours.
sealed class App : Application
{
    MainWindow? window;

    public App() => UnhandledException += (_, e) => e.Handled = true;

    protected override void OnLaunched(LaunchActivatedEventArgs args)
    {
        Resources.MergedDictionaries.Add(new XamlControlsResources());
        window = new MainWindow();
        window.Activate();
    }

    [STAThread]
    static void Main()
    {
        WinRT.ComWrappersSupport.InitializeComWrappers();
        Start(p =>
        {
            var ctx = new Microsoft.UI.Dispatching.DispatcherQueueSynchronizationContext(Microsoft.UI.Dispatching.DispatcherQueue.GetForCurrentThread());
            SynchronizationContext.SetSynchronizationContext(ctx);
            _ = new App();
        });
    }
}
