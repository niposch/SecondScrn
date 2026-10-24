using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using SecondScrn.Services;
using SecondScrn.ViewModels;
using SecondScrn.Views;
using uniffi.secondscrn_host;

namespace SecondScrn;

public partial class App : Application
{
    private readonly bool _launchedMinimized;
    private HostService? _host;
    private TrayService? _tray;
    private MainWindow? _main;
    private TrayPopup? _popup;
    private ulong? _promptedPairing;
    private bool _quitting;

    public static new App Current => (App)Application.Current;

    /// <summary>Owner window for pickers (unpackaged apps must provide one).</summary>
    internal IntPtr MainWindowHandle => _main is null ? IntPtr.Zero : WinRT.Interop.WindowNative.GetWindowHandle(_main);
    public MainViewModel ViewModel { get; private set; } = null!;
    internal UpdateService Updates { get; } = new();

    public App(bool startMinimized)
    {
        _launchedMinimized = startMinimized;
        InitializeComponent();
        UnhandledException += (_, e) =>
        {
            // Keep the streaming backend alive if a UI handler throws.
            e.Handled = true;
            System.Diagnostics.Debug.WriteLine(e.Exception);
        };
    }

    protected override void OnLaunched(LaunchActivatedEventArgs args)
    {
        var dispatcher = DispatcherQueue.GetForCurrentThread();
        _host = new HostService(dispatcher);
        ViewModel = new MainViewModel(_host);
        _host.StateChanged += OnStateChanged;
        try
        {
            _host.Start();
        }
        catch (Exception e)
        {
            ShowFatal(e);
            return;
        }

        _tray = new TrayService(ShowPopup, ShowMainWindow, () => ShowMainWindow("settings"), ViewModel.Disconnect, Quit);
        var settings = _host.State?.Settings;
        if (settings is not null) StartupService.Apply(settings.StartWithWindows, settings.StartMinimized);
        // "--minimized" is only passed when Windows starts us at sign-in.
        if (_launchedMinimized)
            _tray.Notify("SecondScrn is running", "Open it from the notification area. Your tablet can connect at any time.");
        else
            ShowMainWindow();
        if (Environment.GetEnvironmentVariable("SECONDSCRN_SHOW_POPUP") == "1") ShowPopup(); // UI testing aid

        _ = CheckForUpdatesAsync();
    }

    private void OnStateChanged(HostState s)
    {
        _tray?.Update(s);
        _main?.ApplyTheme(ViewModel.ElementTheme);
        _popup?.ApplyTheme(ViewModel.ElementTheme);
        if (s.Pairing is null) PairingDialog.CloseIfOpen();
        if (s.Pairing is { } p && _promptedPairing != p.Id)
        {
            _promptedPairing = p.Id;
            _tray?.Notify("Pairing request", $"{p.DeviceName} wants to use this PC as a second screen.");
            ShowMainWindow();
            _ = PairingDialog.ShowAsync(_main!, p, _host!.Host);
        }
    }

    public void ShowMainWindow() => ShowMainWindow(null);

    internal void NotifyStillRunning() => _tray?.Notify(
        "SecondScrn is still running",
        "Closing the window keeps your tablet connected. Use Quit in the tray menu to exit.");

    public void ShowMainWindow(string? page)
    {
        if (_quitting || _host is null) return;
        if (_main is null)
        {
            _main = new MainWindow(ViewModel, () => _quitting);
            _main.Closed += (_, _) => _main = null;
        }
        _main.ApplyTheme(ViewModel.ElementTheme);
        if (page is not null) _main.Navigate(page);
        _main.ShowAndActivate();
    }

    private void ShowPopup()
    {
        if (_quitting || _host is null) return;
        _popup ??= new TrayPopup(ViewModel, () => { _popup?.Hide(); ShowMainWindow(); });
        _popup.ApplyTheme(ViewModel.ElementTheme);
        _popup.ShowNearTray();
    }

    private async Task CheckForUpdatesAsync()
    {
        if (_host?.State?.Settings.CheckForUpdates != true) return;
        try
        {
            var version = await Updates.CheckAsync();
            if (version is not null) _tray?.Notify("Update available", $"SecondScrn {version} is ready to install from Settings.");
        }
        catch (Exception)
        {
            // Offline or no releases yet: updates are optional.
        }
    }

    internal async Task InstallUpdateAsync() => await Updates.ApplyAndRestartAsync(Shutdown);

    public void Quit()
    {
        _quitting = true;
        Shutdown();
        Exit();
    }

    private void Shutdown()
    {
        _popup?.Close();
        _main?.Close();
        _tray?.Dispose();
        _host?.Dispose(); // tells the tablet to reconnect later, releases the virtual monitor
        _host = null;
    }

    private void ShowFatal(Exception e)
    {
        var w = new Window { Title = "SecondScrn" };
        w.Content = new StackPanel
        {
            Margin = new Thickness(24),
            Spacing = 12,
            Children =
            {
                new TextBlock { Text = "SecondScrn could not start", Style = (Style)Resources["SubtitleTextBlockStyle"] },
                new TextBlock { Text = e.Message, TextWrapping = TextWrapping.Wrap, IsTextSelectionEnabled = true },
            },
        };
        w.Activate();
    }
}
