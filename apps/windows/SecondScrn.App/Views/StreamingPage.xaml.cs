using Microsoft.UI.Xaml.Controls;
using SecondScrn.ViewModels;

namespace SecondScrn.Views;

public sealed partial class StreamingPage : Page
{
    public MainViewModel ViewModel => App.Current.ViewModel;

    public StreamingPage() => InitializeComponent();
}
