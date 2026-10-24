using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using SecondScrn.ViewModels;

namespace SecondScrn.Views;

public sealed partial class DisplayPage : Page
{
    public MainViewModel ViewModel => App.Current.ViewModel;

    public DisplayPage() => InitializeComponent();

    private async void OnInstall(object sender, RoutedEventArgs e) => await DriverActions.InstallAsync(XamlRoot);

    private void OnRefresh(object sender, RoutedEventArgs e) => ViewModel.Refresh();

    private async void OnDisplaySettings(object sender, RoutedEventArgs e) =>
        await Windows.System.Launcher.LaunchUriAsync(new Uri("ms-settings:display"));
}
