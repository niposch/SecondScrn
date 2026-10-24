using CommunityToolkit.Mvvm.Input;
using H.NotifyIcon;
using H.NotifyIcon.Core;
using Microsoft.UI.Xaml.Controls;
using Microsoft.Win32;
using uniffi.secondscrn_host;

namespace SecondScrn.Services;

/// <summary>
/// The notification-area icon. Left click opens the quick-control popup;
/// right click shows a conventional menu.
/// </summary>
internal sealed class TrayService : IDisposable
{
    private readonly TaskbarIcon _icon;
    private readonly MenuFlyoutItem _connectItem;
    private string _iconKey = string.Empty;

    public TrayService(Action showPopup, Action openMain, Action openSettings, Action disconnect, Action quit)
    {
        _connectItem = new MenuFlyoutItem { Text = "Disconnect tablet", Command = new RelayCommand(disconnect) };
        var menu = new MenuFlyout();
        menu.Items.Add(new MenuFlyoutItem { Text = "Open SecondScrn", Command = new RelayCommand(openMain) });
        menu.Items.Add(_connectItem);
        menu.Items.Add(new MenuFlyoutItem { Text = "Settings", Command = new RelayCommand(openSettings) });
        menu.Items.Add(new MenuFlyoutSeparator());
        menu.Items.Add(new MenuFlyoutItem { Text = "Quit SecondScrn", Command = new RelayCommand(quit) });

        _icon = new TaskbarIcon
        {
            ToolTipText = "SecondScrn",
            ContextMenuMode = ContextMenuMode.PopupMenu,
            ContextFlyout = menu,
            LeftClickCommand = new RelayCommand(showPopup),
            NoLeftClickDelay = true,
        };
        SetIcon("idle");
        _icon.ForceCreate(enablesEfficiencyMode: false);
    }

    public void Update(HostState state)
    {
        var s = state.Session;
        _connectItem.IsEnabled = s is not null;
        string key;
        string tip;
        if (state.Pairing is { } p)
        {
            key = "warning";
            tip = $"SecondScrn – pairing request from {p.DeviceName}";
        }
        else if (s is null)
        {
            key = state.Driver.State is DriverState.Ready or DriverState.NotApplicable && state.Listening ? "idle" : "error";
            tip = key == "idle" ? "SecondScrn – waiting for a tablet" : "SecondScrn – needs attention";
        }
        else
        {
            key = s.Phase == Phase.Streaming && s.Metrics.Health != Health.Poor ? "connected" : "warning";
            tip = $"SecondScrn – {s.DeviceName}, {s.Width}×{s.Height}";
        }
        SetIcon(key);
        _icon.ToolTipText = tip;
    }

    public void Notify(string title, string message) =>
        _icon.ShowNotification(title, message, NotificationIcon.Info);

    private static bool TaskbarIsLight()
    {
        using var k = Registry.CurrentUser.OpenSubKey(@"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
        return k?.GetValue("SystemUsesLightTheme") is int v && v == 1;
    }

    private void SetIcon(string state)
    {
        var theme = TaskbarIsLight() ? "light" : "dark";
        var key = $"{state}-{theme}";
        if (key == _iconKey) return;
        _iconKey = key;
        var path = Path.Combine(AppContext.BaseDirectory, "Assets", $"tray-{key}.ico");
        _icon.UpdateIcon(new System.Drawing.Icon(path));
    }

    public void Dispose() => _icon.Dispose();
}
