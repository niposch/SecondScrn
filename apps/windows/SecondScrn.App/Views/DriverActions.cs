using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using SecondScrn.Services;

namespace SecondScrn.Views;

internal static class DriverActions
{
    public static async Task InstallAsync(XamlRoot root)
    {
        var error = await DriverSetup.InstallAsync();
        App.Current.ViewModel.Refresh();
        if (error is null) return;
        await new ContentDialog
        {
            XamlRoot = root,
            Title = "Driver setup",
            Content = new TextBlock { Text = error, TextWrapping = TextWrapping.Wrap },
            CloseButtonText = "OK",
        }.ShowAsync();
    }
}
