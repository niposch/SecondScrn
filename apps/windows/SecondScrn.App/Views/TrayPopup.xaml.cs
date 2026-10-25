using System.Runtime.InteropServices;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using SecondScrn.ViewModels;
using Windows.Graphics;

namespace SecondScrn.Views;

/// <summary>
/// Compact quick-control window shown next to the notification area. It is a
/// borderless, always-on-top WinUI window that hides when it loses focus.
/// </summary>
public sealed partial class TrayPopup : Window
{
    private const int Width = 380;
    private const int Height = 400;
    private const int Margin = 12;
    private readonly Action _openMain;

    public MainViewModel ViewModel { get; }

    public TrayPopup(MainViewModel vm, Action openMain)
    {
        ViewModel = vm;
        _openMain = openMain;
        InitializeComponent();
        SystemBackdrop = new Microsoft.UI.Xaml.Media.DesktopAcrylicBackdrop();
        AppWindow.IsShownInSwitchers = false;
        if (AppWindow.Presenter is OverlappedPresenter p)
        {
            p.IsResizable = false;
            p.IsMaximizable = false;
            p.IsMinimizable = false;
            p.IsAlwaysOnTop = true;
            p.SetBorderAndTitleBar(true, false);
        }
        Activated += (_, e) =>
        {
            if (e.WindowActivationState == WindowActivationState.Deactivated) Hide();
        };
        AppWindow.Closing += (s, e) =>
        {
            e.Cancel = true;
            s.Hide();
        };
    }

    public void ApplyTheme(ElementTheme theme) => Root.RequestedTheme = theme;

    public void Hide() => AppWindow.Hide();

    [DllImport("user32.dll")]
    private static extern bool GetCursorPos(out POINT p);

    [DllImport("user32.dll")]
    private static extern uint GetDpiForWindow(IntPtr hwnd);

    [StructLayout(LayoutKind.Sequential)]
    private struct POINT
    {
        public int X, Y;
    }

    /// <summary>
    /// Positions the popup in the corner next to the taskbar on the monitor
    /// under the cursor (the tray icon was just clicked), honouring DPI and
    /// the taskbar's edge.
    /// </summary>
    public void ShowNearTray()
    {
        GetCursorPos(out var cursor);
        var area = DisplayArea.GetFromPoint(new PointInt32(cursor.X, cursor.Y), DisplayAreaFallback.Nearest);
        var hwnd = WinRT.Interop.WindowNative.GetWindowHandle(this);
        var scale = GetDpiForWindow(hwnd) / 96.0;
        // Use the target monitor's scale once the window is there; the first
        // estimate comes from the window's current monitor.
        int w = (int)(Width * scale), h = (int)(Height * scale), m = (int)(Margin * scale);
        var work = area.WorkArea;
        var outer = area.OuterBounds;

        int x = Math.Clamp(cursor.X - w / 2, work.X + m, work.X + work.Width - w - m);
        int y = work.Y + work.Height - h - m; // taskbar at the bottom (default)
        if (work.Y > outer.Y) y = work.Y + m; // taskbar at the top
        if (work.X > outer.X) x = work.X + m; // taskbar on the left
        else if (work.X + work.Width < outer.X + outer.Width) x = work.X + work.Width - w - m; // right
        if (work.Height == outer.Height && work.Width == outer.Width) x = work.X + work.Width - w - m; // auto-hide taskbar

        AppWindow.MoveAndResize(new RectInt32(x, y, w, h));
        AppWindow.Show();
        // Rescale if the popup landed on a monitor with a different DPI.
        var actual = GetDpiForWindow(hwnd) / 96.0;
        if (Math.Abs(actual - scale) > 0.01)
        {
            AppWindow.Resize(new SizeInt32((int)(Width * actual), (int)(Height * actual)));
        }
        Activate();
    }

    private void OnDisconnect(object sender, RoutedEventArgs e) => ViewModel.Disconnect();

    private void OnOpen(object sender, RoutedEventArgs e) => _openMain();
}
