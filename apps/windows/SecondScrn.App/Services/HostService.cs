using Microsoft.UI.Dispatching;
using uniffi.secondscrn_host;

namespace SecondScrn.Services;

/// <summary>
/// Owns the Rust host service. State snapshots arrive on a Rust thread and are
/// marshalled to the UI thread. The service keeps running while windows are
/// hidden; only <see cref="Dispose"/> stops it.
/// </summary>
internal sealed class HostService : HostListener, IDisposable
{
    private readonly DispatcherQueue _dispatcher;
    private SecondScrnHost? _host;

    public event Action<HostState>? StateChanged;
    public HostState? State { get; private set; }

    public HostService(DispatcherQueue dispatcher)
    {
        _dispatcher = dispatcher;
    }

    public SecondScrnHost Host => _host ?? throw new InvalidOperationException("Host not started");

    public void Start()
    {
        _host = SecondScrnHost.Start(string.Empty, this);
        State = _host.State();
        StateChanged?.Invoke(State);
    }

    public void OnStateChanged(HostState state)
    {
        _dispatcher.TryEnqueue(() =>
        {
            State = state;
            StateChanged?.Invoke(state);
        });
    }

    public void Dispose()
    {
        try
        {
            _host?.Shutdown();
            _host?.Dispose();
        }
        catch (Exception)
        {
            // Shutting down; nothing useful to report.
        }
        _host = null;
    }
}
