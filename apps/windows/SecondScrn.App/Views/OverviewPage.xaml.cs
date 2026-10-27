using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using SecondScrn.Services;
using SecondScrn.ViewModels;

namespace SecondScrn.Views;

public sealed partial class OverviewPage : Page
{
    public MainViewModel ViewModel => App.Current.ViewModel;

    public OverviewPage() => InitializeComponent();

    private void OnDisconnect(object sender, RoutedEventArgs e) => ViewModel.Disconnect();

    private void OnResetPreset(object sender, RoutedEventArgs e) => ViewModel.ResetPreset();

    private async void OnTabletApp(object sender, RoutedEventArgs e)
    {
        if (ViewModel.TabletAppUrl is { } url) await Windows.System.Launcher.LaunchUriAsync(new Uri(url));
    }

    /// Updates this PC to exactly the tablet's version.
    private async void OnInstallTabletVersion(object sender, RoutedEventArgs e)
    {
        if (ViewModel.TabletVersion is not { } version || sender is not Button button) return;
        button.IsEnabled = false;
        var installed = false;
        try
        {
            installed = await App.Current.InstallVersionAsync(version);
        }
        catch (Exception)
        {
            // Offline or not published: explained below.
        }
        if (!installed)
        {
            button.IsEnabled = true;
            await new ContentDialog
            {
                XamlRoot = XamlRoot,
                Title = $"SecondScrn {version} can't be installed here",
                Content = App.Current.Updates.IsInstalled
                    ? $"The release {version} wasn't found or isn't newer than this PC's version. Download it from {UpdateService.RepositoryUrl}/releases."
                    : "This is a development build; it isn't updated automatically.",
                CloseButtonText = "OK",
            }.ShowAsync();
        }
    }

    private async void OnDisplaySettings(object sender, RoutedEventArgs e) =>
        await Windows.System.Launcher.LaunchUriAsync(new Uri("ms-settings:display"));

    private void OnDiagnostics(object sender, RoutedEventArgs e) => App.Current.ShowMainWindow("diagnostics");

    private async void OnInstallDriver(object sender, RoutedEventArgs e) => await DriverActions.InstallAsync(XamlRoot);
}
