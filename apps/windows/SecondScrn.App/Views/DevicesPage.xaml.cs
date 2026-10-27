using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using SecondScrn.ViewModels;

namespace SecondScrn.Views;

public sealed partial class DevicesPage : Page
{
    public MainViewModel ViewModel => App.Current.ViewModel;
    public Visibility NoDevicesVisibility => ViewModel.Devices.Count == 0 ? Visibility.Visible : Visibility.Collapsed;
    public Visibility HasDevicesVisibility => ViewModel.Devices.Count == 0 ? Visibility.Collapsed : Visibility.Visible;

    public DevicesPage()
    {
        InitializeComponent();
        ViewModel.Devices.CollectionChanged += (_, _) => Bindings.Update();
    }

    private void OnPresetChanged(object sender, SelectionChangedEventArgs e)
    {
        if (sender is ComboBox { Tag: DeviceItem d } c && c.SelectedIndex >= 0 && c.SelectedIndex != d.PresetIndex)
        {
            d.PresetIndex = c.SelectedIndex;
            ViewModel.SetDevicePreferences(d);
        }
    }

    private void OnAutoReconnectToggled(object sender, RoutedEventArgs e)
    {
        if (sender is ToggleSwitch { Tag: DeviceItem d } t && t.IsOn != d.AutoReconnect)
        {
            d.AutoReconnect = t.IsOn;
            ViewModel.SetDevicePreferences(d);
        }
    }

    private async void OnRemove(object sender, RoutedEventArgs e)
    {
        if (sender is not Button { Tag: DeviceItem d }) return;
        var dialog = new ContentDialog
        {
            XamlRoot = XamlRoot,
            Title = $"Remove {d.Name}?",
            Content = "The tablet is disconnected and must be paired again before it can connect.",
            PrimaryButtonText = "Remove",
            CloseButtonText = "Cancel",
            DefaultButton = ContentDialogButton.Close,
        };
        if (await dialog.ShowAsync() == ContentDialogResult.Primary) ViewModel.Revoke(d.Fingerprint);
    }
}
