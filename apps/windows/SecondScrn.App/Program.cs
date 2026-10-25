using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.Windows.AppLifecycle;
using Velopack;

namespace SecondScrn;

public static class Program
{
    [STAThread]
    private static int Main(string[] args)
    {
        // Velopack install/update hooks must run before anything else.
        VelopackApp.Build()
            // Clean uninstall: remove the "start with Windows" entry. The
            // virtual display driver is left alone (other apps may use it).
            .OnBeforeUninstallFastCallback(_ =>
            {
                Services.StartupService.Apply(false, false);
                // Pairings and the PC identity go with the app.
                var data = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "SecondScrnData");
                try { if (Directory.Exists(data)) Directory.Delete(data, recursive: true); } catch (IOException) { }
            })
            .Run();
        WinRT.ComWrappersSupport.InitializeComWrappers();

        // Single instance: a second launch activates the running app.
        var main = AppInstance.FindOrRegisterForKey("SecondScrn");
        if (!main.IsCurrent)
        {
            var activation = AppInstance.GetCurrent().GetActivatedEventArgs();
            main.RedirectActivationToAsync(activation).AsTask().Wait();
            return 0;
        }

        Application.Start(_ =>
        {
            var context = new DispatcherQueueSynchronizationContext(DispatcherQueue.GetForCurrentThread());
            SynchronizationContext.SetSynchronizationContext(context);
            var app = new App(args.Contains("--minimized"));
            var queue = DispatcherQueue.GetForCurrentThread();
            // Raised on a background thread when the user launches us again.
            main.Activated += (_, _) => queue.TryEnqueue(app.ShowMainWindow);
        });
        return 0;
    }
}
