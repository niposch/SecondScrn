using System.Diagnostics;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using SecondScrn.ViewModels;
using Windows.Storage.Pickers;

namespace SecondScrn.Views;

public sealed partial class DiagnosticsPage : Page
{
    private readonly DispatcherTimer _timer = new() { Interval = TimeSpan.FromSeconds(2) };

    public MainViewModel ViewModel => App.Current.ViewModel;

    public DiagnosticsPage()
    {
        InitializeComponent();
        _timer.Tick += (_, _) => RefreshLogs();
        Loaded += (_, _) =>
        {
            RefreshLogs();
            _timer.Start();
        };
        Unloaded += (_, _) => _timer.Stop();
    }

    private void RefreshLogs()
    {
        var text = string.Join(Environment.NewLine, ViewModel.RecentLogs(400));
        if (text != Logs.Text) Logs.Text = text;
    }

    private async void OnExport(object sender, RoutedEventArgs e)
    {
        var picker = new FileSavePicker { SuggestedFileName = $"secondscrn-diagnostics-{DateTime.Now:yyyyMMdd-HHmm}" };
        picker.FileTypeChoices.Add("JSON", [".json"]);
        WinRT.Interop.InitializeWithWindow.Initialize(picker, App.Current.MainWindowHandle);
        var file = await picker.PickSaveFileAsync();
        if (file is null) return;
        await File.WriteAllTextAsync(file.Path, ViewModel.ExportDiagnostics());
    }

    private void OnBenchmark(object sender, RoutedEventArgs e) => ViewModel.RunBenchmark();

    private void OnOpenLogs(object sender, RoutedEventArgs e)
    {
        var dir = Path.Combine(ViewModel.DataDirectory, "logs");
        Directory.CreateDirectory(dir);
        Process.Start(new ProcessStartInfo { FileName = dir, UseShellExecute = true });
    }
}
