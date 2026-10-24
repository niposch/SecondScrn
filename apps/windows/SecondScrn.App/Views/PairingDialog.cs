using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using uniffi.secondscrn_host;

namespace SecondScrn.Views;

/// <summary>
/// Asks the PC user for the code shown on the tablet. Typing the code is the
/// explicit authorization of the tablet.
/// </summary>
internal static class PairingDialog
{
    private static bool _open;
    private static ContentDialog? _dialog;

    /// <summary>The tablet cancelled or the request expired.</summary>
    public static void CloseIfOpen() => _dialog?.Hide();

    public static async Task ShowAsync(Window owner, PairingView request, SecondScrnHost host)
    {
        if (_open || owner.Content?.XamlRoot is not { } root) return;
        _open = true;
        try
        {
            var code = new TextBox
            {
                // Header as a template so the large digit font doesn't apply to it.
                Header = new TextBlock { Text = "Code shown on the tablet" },
                PlaceholderText = "123 456",
                MaxLength = 7,
                FontSize = 28,
                Width = 220,
                HorizontalAlignment = HorizontalAlignment.Left,
                InputScope = new Microsoft.UI.Xaml.Input.InputScope
                {
                    Names = { new Microsoft.UI.Xaml.Input.InputScopeName(Microsoft.UI.Xaml.Input.InputScopeNameValue.Number) },
                },
            };
            var error = new TextBlock { Foreground = (Microsoft.UI.Xaml.Media.Brush)Application.Current.Resources["SystemFillColorCriticalBrush"], TextWrapping = TextWrapping.Wrap };
            var dialog = new ContentDialog
            {
                XamlRoot = root,
                Title = $"Pair {request.DeviceName}?",
                PrimaryButtonText = "Pair",
                SecondaryButtonText = "Decline",
                DefaultButton = ContentDialogButton.Primary,
                Content = new StackPanel
                {
                    Spacing = 12,
                    MaxWidth = 440,
                    Children =
                    {
                        new TextBlock
                        {
                            TextWrapping = TextWrapping.Wrap,
                            Text = $"{request.DeviceName} ({request.DeviceModel}) at {request.RemoteAddress} wants to use this PC as a second screen " +
                                   "and control it with touch. Enter the code shown on the tablet to allow it.",
                        },
                        code,
                        new TextBlock
                        {
                            Text = $"Tablet identity {request.FingerprintShort}",
                            Style = (Style)Application.Current.Resources["Caption"],
                        },
                        error,
                    },
                },
            };
            dialog.PrimaryButtonClick += (_, args) =>
            {
                try
                {
                    host.SubmitPairingCode(request.Id, code.Text);
                }
                catch (HostException.Failed e)
                {
                    error.Text = e.detail;
                    args.Cancel = true;
                }
                catch (Exception e)
                {
                    // The request expired or the tablet gave up.
                    error.Text = e.Message;
                }
            };
            _dialog = dialog;
            var result = await dialog.ShowAsync();
            if (result != ContentDialogResult.Primary) host.RejectPairing(request.Id);
        }
        finally
        {
            _dialog = null;
            _open = false;
        }
    }
}
