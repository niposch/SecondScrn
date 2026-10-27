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

    private readonly UpdateManager _manager = new(CreateSource());

    /// <summary>
    /// SECONDSCRN_UPDATE_SOURCE overrides the feed with a local folder or URL
    /// (used to test update packages before publishing them).
    /// </summary>
    private static IUpdateSource CreateSource()
    {
        var overrideSource = Environment.GetEnvironmentVariable("SECONDSCRN_UPDATE_SOURCE");
        if (string.IsNullOrWhiteSpace(overrideSource)) return new GithubSource(RepositoryUrl, null, false);
        return Directory.Exists(overrideSource)
            ? new SimpleFileSource(new DirectoryInfo(overrideSource))
            : new SimpleWebSource(overrideSource);
    }
    private UpdateInfo? _pending;

    /// <summary>False for development builds that weren't installed by Velopack.</summary>
    public bool IsInstalled => _manager.IsInstalled;

    public string? AvailableVersion => _pending?.TargetFullRelease.Version.ToString();

    public async Task<string?> CheckAsync()
    {
        if (!_manager.IsInstalled) return null;
        _exact = null;
        _pending = await _manager.CheckForUpdatesAsync();
        return AvailableVersion;
    }

    /// Release assets of exactly `version` (the PC and tablet apps are
    /// updated in lockstep before 1.0: the older side gets the other's
    /// version, not necessarily the newest).
    public static string ReleaseUrl(string version) => $"{RepositoryUrl}/releases/download/v{version}/";

    /// The tablet app matching `version`.
    public static string TabletAppUrl(string version) => $"{ReleaseUrl(version)}SecondScrn-{version}-android.apk";

    /// Prepares an update to exactly `version` (instead of the newest).
    /// Returns false when that release isn't an update for this install.
    public async Task<bool> CheckVersionAsync(string version)
    {
        if (!_manager.IsInstalled) return false;
        var exact = new UpdateManager(new SimpleWebSource(ReleaseUrl(version)));
        var info = await exact.CheckForUpdatesAsync();
        if (info is null || info.TargetFullRelease.Version.ToString() != version) return false;
        _exact = exact;
        _pending = info;
        return true;
    }

    private UpdateManager? _exact;

    /// <summary>Downloads the update and restarts into it.</summary>
    public async Task ApplyAndRestartAsync(Action beforeRestart)
    {
        if (_pending is null) return;
        var manager = _exact ?? _manager;
        await manager.DownloadUpdatesAsync(_pending);
        beforeRestart();
        manager.ApplyUpdatesAndRestart(_pending);
    }
}
