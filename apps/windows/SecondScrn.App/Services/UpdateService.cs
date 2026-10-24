using Velopack;
using Velopack.Sources;

namespace SecondScrn.Services;

/// <summary>
/// Application updates through Velopack. The virtual display driver is not
/// part of these updates; it is installed separately (see DriverSetup).
/// </summary>
internal sealed class UpdateService
{
    /// <summary>Release feed. GitHub releases produced by the packaging workflow.</summary>
    public const string RepositoryUrl = "https://github.com/niposch/SecondScrn";

    private readonly UpdateManager _manager = new(new GithubSource(RepositoryUrl, null, false));
    private UpdateInfo? _pending;

    /// <summary>False for development builds that weren't installed by Velopack.</summary>
    public bool IsInstalled => _manager.IsInstalled;

    public string? AvailableVersion => _pending?.TargetFullRelease.Version.ToString();

    public async Task<string?> CheckAsync()
    {
        if (!_manager.IsInstalled) return null;
        _pending = await _manager.CheckForUpdatesAsync();
        return AvailableVersion;
    }

    /// <summary>Downloads the update and restarts into it.</summary>
    public async Task ApplyAndRestartAsync(Action beforeRestart)
    {
        if (_pending is null) return;
        await _manager.DownloadUpdatesAsync(_pending);
        beforeRestart();
        _manager.ApplyUpdatesAndRestart(_pending);
    }
}
