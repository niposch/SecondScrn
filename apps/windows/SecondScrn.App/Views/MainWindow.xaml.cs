using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using SecondScrn.ViewModels;
using Windows.Graphics;

namespace SecondScrn.Views;

public sealed partial class MainWindow : Window
{
    private readonly MainViewModel _vm;
    private readonly Func<bool> _quitting;
    private bool _toldAboutTray;

    public MainWindow(MainViewModel vm, Func<bool> quitting)
    {
        _vm = vm;
        _quitting = quitting;
        InitializeComponent();
        ExtendsContentIntoTitleBar = true;
        SetTitleBar(TitleBar);
        SystemBackdrop = new Microsoft.UI.Xaml.Media.MicaBackdrop();
        AppWindow.SetIcon(Path.Combine(AppContext.BaseDirectory, "Assets", "SecondScrn.ico"));
        var scale = Content.XamlRoot?.RasterizationScale ?? 1.0;
        AppWindow.Resize(new SizeInt32((int)(1100 * scale), (int)(760 * scale)));
        AppWindow.Closing += OnClosing;
        Nav.SelectedItem = Nav.MenuItems[0];
    }

    private void OnClosing(AppWindow sender, AppWindowClosingEventArgs args)
    {
        if (_quitting() || !_vm.CloseToTray) return;
        // Closing hides the window; streaming continues in the background.
        args.Cancel = true;
        sender.Hide();
        if (!_toldAboutTray)
        {
            _toldAboutTray = true;
            App.Current.NotifyStillRunning();
        }
    }

    public void ShowAndActivate()
    {
        AppWindow.Show();
        if (AppWindow.Presenter is OverlappedPresenter { State: OverlappedPresenterState.Minimized } p) p.Restore();
        Activate();
    }

    public void ApplyTheme(ElementTheme theme) => Root.RequestedTheme = theme;

    public void Navigate(string tag)
    {
        if (tag == "settings")
        {
            Nav.SelectedItem = Nav.SettingsItem;
            return;
        }
        Nav.SelectedItem = Nav.MenuItems.OfType<NavigationViewItem>().FirstOrDefault(i => (string)i.Tag == tag);
    }

    private void OnNavigationChanged(NavigationView sender, NavigationViewSelectionChangedEventArgs args)
    {
        var tag = args.IsSettingsSelected ? "settings" : (args.SelectedItem as NavigationViewItem)?.Tag as string;
        var page = tag switch
        {
            "devices" => typeof(DevicesPage),
            "display" => typeof(DisplayPage),
            "streaming" => typeof(StreamingPage),
            "diagnostics" => typeof(DiagnosticsPage),
            "settings" => typeof(SettingsPage),
            _ => typeof(OverviewPage),
        };
        ContentFrame.Navigate(page);
    }
}
