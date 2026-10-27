using System.Diagnostics;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using SecondScrn.ViewModels;

namespace SecondScrn.Views;

public sealed partial class SettingsPage : Page
{
    public MainViewModel ViewModel => App.Current.ViewModel;

    public SettingsPage()
    {
        InitializeComponent();
        if (!App.Current.Updates.IsInstalled)
        {
            CheckButton.IsEnabled = false;
            UpdateStatus.Text = "Updates are available in installed builds only.";
        }
        else if (App.Current.Updates.AvailableVersion is { } v)
        {
            ShowAvailable(v);
        }
    }

    private void ShowAvailable(string version)
    {
        UpdateStatus.Text = $"Version {version} is available. Update the tablet app to {version} as well: both must run the same version.";
        InstallButton.Visibility = Visibility.Visible;
    }

    private async void OnCheckUpdates(object sender, RoutedEventArgs e)
    {
        CheckButton.IsEnabled = false;
        UpdateStatus.Text = "Checking…";
        try
        {
            var v = await App.Current.Updates.CheckAsync();
            if (v is null) UpdateStatus.Text = "SecondScrn is up to date.";
            else ShowAvailable(v);
        }
        catch (Exception ex)
        {
            UpdateStatus.Text = $"Could not check for updates: {ex.Message}";
        }
        CheckButton.IsEnabled = true;
    }

    private async void OnInstallUpdate(object sender, RoutedEventArgs e)
    {
        InstallButton.IsEnabled = false;
        UpdateStatus.Text = "Downloading…";
        try
        {
            await App.Current.InstallUpdateAsync();
        }
        catch (Exception ex)
        {
            UpdateStatus.Text = $"Update failed: {ex.Message}";
            InstallButton.IsEnabled = true;
        }
    }

    private static void Open(string file)
    {
        var path = Path.Combine(AppContext.BaseDirectory, file);
        if (File.Exists(path)) Process.Start(new ProcessStartInfo { FileName = path, UseShellExecute = true });
    }

    private void OnReconnect(object sender, RoutedEventArgs e) => ViewModel.Reconnect();

    private void OnRestart(object sender, RoutedEventArgs e) => App.Current.Restart();

    private async void OnInstallDriver(object sender, RoutedEventArgs e) => await DriverActions.InstallAsync(XamlRoot);

    private void OnRefresh(object sender, RoutedEventArgs e) => ViewModel.Refresh();

    private void OnLicense(object sender, RoutedEventArgs e) => Open("LICENSE.txt");

    private void OnNotices(object sender, RoutedEventArgs e) => Open("THIRD_PARTY_NOTICES.md");
}
