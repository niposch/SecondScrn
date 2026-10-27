using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using SecondScrn.ViewModels;

namespace SecondScrn.Views;

public sealed partial class StreamingPage : Page
{
    public MainViewModel ViewModel => App.Current.ViewModel;

    public StreamingPage() => InitializeComponent();

    private void OnRestart(object sender, RoutedEventArgs e) => App.Current.Restart();

    private void OnBenchmark(object sender, RoutedEventArgs e) => ViewModel.RunBenchmark();
}
