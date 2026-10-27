using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.Windows.AppLifecycle;
using Velopack;

namespace SecondScrn;

public static class Program
{
    /// <summary>Argument of a restarted instance: the previous one's process ID.</summary>
    internal const string RestartedFrom = "--restarted-from";

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

        // A restart: let the previous instance release the single-instance
        // key (and the virtual monitor) first.
        var i = Array.IndexOf(args, RestartedFrom);
        if (i >= 0 && i + 1 < args.Length && int.TryParse(args[i + 1], out var pid))
        {
            try
            {
                using var previous = System.Diagnostics.Process.GetProcessById(pid);
                previous.WaitForExit(TimeSpan.FromSeconds(20));
            }
            catch (ArgumentException)
            {
                // Already gone.
            }
        }

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
