using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using SecondScrn.ViewModels;

namespace SecondScrn.Views;

public sealed partial class OverviewPage : Page
{
    public MainViewModel ViewModel => App.Current.ViewModel;

    public OverviewPage() => InitializeComponent();

    private void OnDisconnect(object sender, RoutedEventArgs e) => ViewModel.Disconnect();

    private async void OnDisplaySettings(object sender, RoutedEventArgs e) =>
        await Windows.System.Launcher.LaunchUriAsync(new Uri("ms-settings:display"));

    private void OnDiagnostics(object sender, RoutedEventArgs e) => App.Current.ShowMainWindow("diagnostics");

    private async void OnInstallDriver(object sender, RoutedEventArgs e) => await DriverActions.InstallAsync(XamlRoot);
}
