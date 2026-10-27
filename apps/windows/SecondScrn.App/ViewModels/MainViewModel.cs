using System.Collections.ObjectModel;
using System.Globalization;
using CommunityToolkit.Mvvm.ComponentModel;
using Microsoft.UI.Xaml;
using SecondScrn.Services;
using uniffi.secondscrn_host;

namespace SecondScrn.ViewModels;

public sealed class DeviceItem
{
    public string Fingerprint { get; init; } = "";
    public string Name { get; init; } = "";
    public string Details { get; init; } = "";
    public bool Connected { get; init; }
    public bool AutoReconnect { get; set; }
    public int PresetIndex { get; set; }
    public Visibility ConnectedVisibility => Connected ? Visibility.Visible : Visibility.Collapsed;
}

/// <summary>
/// Everything the windows display, derived from the latest host snapshot.
/// Both the main window and the tray popup bind to this single instance.
/// </summary>
public sealed partial class MainViewModel : ObservableObject
{
    private readonly HostService _host;
    private HostState? _s;
    private bool _updating;

    internal MainViewModel(HostService host)
    {
        _host = host;
        host.StateChanged += Update;
    }

    internal HostState? State => _s;
    public ObservableCollection<DeviceItem> Devices { get; } = new();

    private void Update(HostState s)
    {
        _s = s;
        _updating = true;
        try
        {
            TrackSessionStart(s);
            SyncDevices(s);
            OnPropertyChanged(string.Empty); // refresh every binding
        }
        finally
        {
            _updating = false;
        }
    }

    /// <summary>Rebuilds the list only when something in it changed: state
    /// arrives several times a second, and a rebuild closes open drop-downs.</summary>
    private void SyncDevices(HostState s)
    {
        var items = s.Devices.OrderByDescending(d => d.LastSeen ?? 0).Select(d => new DeviceItem
        {
            Fingerprint = d.Fingerprint,
            Name = d.Name,
            Details = $"{d.Model} · ID {d.FingerprintShort} · paired {FormatAge(d.PairedAt)} · " +
                      (d.LastSeen is { } t ? $"last connected {FormatAge(t)}" : "never connected"),
            Connected = d.Connected,
            AutoReconnect = d.AutoReconnect,
            PresetIndex = d.Preset is { } p ? (int)p + 1 : 0,
        }).ToList();
        static object Key(DeviceItem d) => (d.Fingerprint, d.Name, d.Details, d.Connected, d.AutoReconnect, d.PresetIndex);
        if (items.Select(Key).SequenceEqual(Devices.Select(Key))) return;
        Devices.Clear();
        foreach (var d in items) Devices.Add(d);
    }

    // Settings that apply when a tablet connects, as they were when the
    // current session started.
    private (ulong Since, VideoCodec? Codec, uint Bitrate, uint? FpsLimit, GpuKind? Gpu)? _sessionStart;

    private void TrackSessionStart(HostState s)
    {
        if (s.Session is not { } x)
        {
            _sessionStart = null;
            return;
        }
        if (_sessionStart?.Since != x.ConnectedSince)
            _sessionStart = (x.ConnectedSince, s.Settings.PreferredCodec, s.Settings.MaxBitrateMbps, s.Settings.FpsLimit, s.Settings.EncoderGpu);
    }

    /// <summary>A setting that applies to new connections changed during this one.</summary>
    public bool NeedsReconnect => _sessionStart is { } start && _s is { } s &&
        (start.Codec, start.Bitrate, start.FpsLimit, start.Gpu) !=
        (s.Settings.PreferredCodec, s.Settings.MaxBitrateMbps, s.Settings.FpsLimit, s.Settings.EncoderGpu);

    internal void Reconnect() => _host.Host.Reconnect();

    private static string FormatAge(ulong unixSeconds)
    {
        var t = DateTimeOffset.FromUnixTimeSeconds((long)unixSeconds).ToLocalTime();
        var age = DateTimeOffset.Now - t;
        if (age < TimeSpan.FromMinutes(1)) return "just now";
        if (age < TimeSpan.FromHours(1)) return $"{(int)age.TotalMinutes} min ago";
        if (age < TimeSpan.FromDays(1)) return $"{(int)age.TotalHours} h ago";
        return t.ToString("d");
    }

    private static string Codec(VideoCodec? c) => c switch
    {
        VideoCodec.H264 => "H.264",
        VideoCodec.H265 => "HEVC",
        VideoCodec.Av1 => "AV1",
        _ => "–",
    };

    // ---- Overview / tray ---------------------------------------------------

    public bool IsConnected => _s?.Session is not null;
    public Visibility ConnectedVisibility => IsConnected ? Visibility.Visible : Visibility.Collapsed;
    public Visibility IdleVisibility => IsConnected ? Visibility.Collapsed : Visibility.Visible;

    public string HostName => _s?.HostName ?? "";
    public string HostIdentity => _s is null ? "" : $"PC identity {_s.FingerprintShort}";
    public string Version => _s is null ? "" : $"Version {_s.Version}";

    public string ConnectionTitle => _s?.Session switch
    {
        null when _s?.Pairing is { } p => $"Pairing request from {p.DeviceName}",
        null => "No tablet connected",
        { Phase: Phase.Negotiating } s => $"Connecting {s.DeviceName}…",
        { Phase: Phase.Interrupted } s => $"{s.DeviceName}: connection interrupted",
        var s => $"Connected to {s.DeviceName}",
    };

    public string ConnectionDetail
    {
        get
        {
            if (_s is null) return "Starting…";
            if (_s.Session is { } s)
                return $"{s.Width}×{s.Height} · up to {s.MaxFps} fps · {Codec(s.Codec)} ({s.Encoder}) · {s.RemoteAddress}";
            if (!_s.Listening) return _s.LastError ?? "Not accepting connections.";
            return $"Open SecondScrn on your tablet and choose “{_s.HostName}”. " +
                   $"Manual address: {string.Join(" or ", _s.Addresses)} (port {_s.Port}).";
        }
    }

    public string HealthText => _s?.Session?.Metrics.Health switch
    {
        Health.Good => "Excellent connection",
        Health.Fair => "Fair connection",
        Health.Poor => "Poor connection",
        _ => IsConnected ? "Measuring…" : "",
    };

    /// <summary><paramref name="v"/> right-aligned in <paramref name="width"/>
    /// digit widths (padded with figure spaces), so changing numbers don't
    /// move the text around them.</summary>
    private static string Fixed(double v, string format, int width) =>
        v.ToString(format, CultureInfo.CurrentCulture).PadLeft(width, ' ');

    public string ResolutionText => _s?.Session is { } s ? $"{s.Width} × {s.Height}" : "–";
    /// What the tablet shows when it reports it; what the PC sends otherwise.
    public string FpsText => _s?.Session is not { } s ? "–"
        : $"{Fixed(s.Metrics.ClientFps ?? s.Metrics.EncodeFps, "0", 3)} fps";
    public string FpsDetail => _s?.Session is not { } s ? ""
        : s.Metrics.ClientFps is not null
            ? $"shown · sending {Fixed(s.Metrics.EncodeFps, "0", 3)} · target {Fixed(s.Metrics.TargetFps, "0", 3)}"
            : $"sending · target {Fixed(s.Metrics.TargetFps, "0", 3)}";
    public string BitrateText => _s?.Session is { } s ? $"{Fixed(s.Metrics.SendMbps, "0.0", 5)} Mbit/s" : "–";
    public string BitrateDetail => _s?.Session is { } s ? $"encoder target {Fixed(s.Metrics.TargetMbps, "0.0", 5)}" : "";
    public string LatencyText => _s?.Session?.Metrics.RttMs is { } r ? $"{Fixed(r, "0", 3)} ms" : "–";
    public string LossText => _s?.Session?.Metrics.LossPercent is { } l ? $"{Fixed(l, "0.0", 4)} %" : "–";
    public string CodecText => _s?.Session is { } s ? Codec(s.Codec) : "–";
    public string QualityText => PresetsData[PresetIndex];
    public string[] Presets => PresetsData;
    public string[] Codecs => CodecsData;
    public string[] Themes => ThemesData;

    private static readonly string[] PresetsData = ["Automatic", "High quality", "Smooth", "Weak Wi-Fi"];

    private static readonly string[] PresetHints =
    [
        "Full resolution at 60 fps; the bitrate follows the network.",
        "About twice the bitrate for sharper text and video, slightly more latency.",
        "75 % resolution at up to 120 fps.",
        "67 % resolution at 30 fps with a low bitrate.",
    ];

    /// <summary>The connected tablet's preset, remembered for that tablet
    /// (the default preset when none is connected).</summary>
    public int PresetIndex
    {
        get => (int)(_s?.Session?.Preset ?? _s?.Settings.Preset ?? Preset.Automatic);
        set
        {
            if (_updating || _s?.Session is null || value < 0 || value == PresetIndex) return;
            SetSessionPreset((Preset)value);
        }
    }

    public string PresetHint => PresetHints[PresetIndex];

    /// <summary>The network can't carry the chosen resolution and frame rate.</summary>
    public bool NetworkLimited => _s?.Session?.NetworkLimited == true;

    public string NetworkLimitedText => _s?.Session is { } s
        ? $"The network can't carry {s.Width} × {s.Height} at {s.MaxFps} fps right now, so fewer frames are sent. Choose a lower resolution or frame rate below."
        : "";

    public Visibility PresetChangedVisibility => _s?.Session?.Custom == true ? Visibility.Visible : Visibility.Collapsed;

    public string PresetChangedText => $"Resolution or frame rate changed from “{PresetsData[PresetIndex]}”.";

    /// <summary>Back to the preset's resolution and frame rate.</summary>
    internal void ResetPreset()
    {
        if (_s?.Session is { } s) SetSessionPreset(s.Preset);
    }

    private void SetSessionPreset(Preset preset)
    {
        try
        {
            _host.Host.SetSessionPreset(preset);
        }
        catch (Exception)
        {
            // The tablet disconnected meanwhile.
        }
    }

    /// <summary>For tablets without a preset of their own.</summary>
    public int DefaultPresetIndex
    {
        get => (int)(_s?.Settings.Preset ?? Preset.Automatic);
        set
        {
            if (_updating || _s is null || value < 0 || value == DefaultPresetIndex) return;
            _host.Host.SetPreset((Preset)value);
        }
    }

    // ---- Resolution of the connected tablet -------------------------------

    private string[] _resolutionLabels = [];

    /// <summary>Offered sizes; the same array instance while they don't change,
    /// so the combo boxes keep their selection across snapshots.</summary>
    public string[] ResolutionLabels
    {
        get
        {
            var labels = _s?.Session?.Resolutions.Select(r => r.Label).ToArray() ?? [];
            if (!labels.SequenceEqual(_resolutionLabels)) _resolutionLabels = labels;
            return _resolutionLabels;
        }
    }

    public int ResolutionIndex
    {
        get => _s?.Session is { } s ? s.Resolutions.ToList().FindIndex(r => r.ScalePercent == s.ScalePercent) : -1;
        set
        {
            if (_updating || _s?.Session is not { } s || value < 0 || value >= s.Resolutions.Count() || value == ResolutionIndex) return;
            try
            {
                _host.Host.SetResolution(s.Resolutions.ElementAt(value).ScalePercent);
            }
            catch (Exception)
            {
                // The tablet disconnected meanwhile.
            }
        }
    }

    private string[] _frameRateLabels = [];

    public string[] FrameRateLabels
    {
        get
        {
            var labels = _s?.Session?.FrameRates.Select(r => $"{r} fps").ToArray() ?? [];
            if (!labels.SequenceEqual(_frameRateLabels)) _frameRateLabels = labels;
            return _frameRateLabels;
        }
    }

    public int FrameRateIndex
    {
        get => _s?.Session is { } s ? s.FrameRates.ToList().IndexOf(s.MaxFps) : -1;
        set
        {
            if (_updating || _s?.Session is not { } s || value < 0 || value >= s.FrameRates.Count() || value == FrameRateIndex) return;
            try
            {
                _host.Host.SetFrameRate(s.FrameRates.ElementAt(value));
            }
            catch (Exception)
            {
                // The tablet disconnected meanwhile.
            }
        }
    }

    public bool CanChangeResolution => _s?.Session is { Phase: Phase.Streaming } s && s.Resolutions.Any();

    public string ResolutionHint => _s?.Session switch
    {
        null => "Available while a tablet is connected.",
        { ScalePercent: 0 } s => $"{s.Width} × {s.Height} was chosen in the Windows display settings. Pick a size to return to one that fills the tablet.",
        _ => "Lower resolutions need less bandwidth; the tablet scales the picture up. " +
             "Changes are remembered for this tablet and can also be made on the tablet.",
    };

    public string? LastError => _s?.LastError;

    // ---- Version mismatch (before 1.0 both apps run the same version) -------

    public bool HasVersionMismatch => _s?.VersionMismatch is not null;

    public string VersionMismatchText => _s?.VersionMismatch is not { } m ? ""
        : m.TabletOlder
            ? $"{m.DeviceName} runs SecondScrn {m.TabletVersion}, this PC {m.HostVersion}. Both must run the same version: install SecondScrn {m.HostVersion} on the tablet ({m.TabletAppUrl})."
            : $"{m.DeviceName} runs SecondScrn {m.TabletVersion}, this PC {m.HostVersion}. Both must run the same version: install SecondScrn {m.TabletVersion} on this PC.";

    public Visibility InstallTabletVersionVisibility =>
        _s?.VersionMismatch is { TabletOlder: false } ? Visibility.Visible : Visibility.Collapsed;

    public string InstallTabletVersionLabel => _s?.VersionMismatch is { } m ? $"Install SecondScrn {m.TabletVersion}" : "";

    public Visibility TabletAppLinkVisibility =>
        _s?.VersionMismatch is { TabletOlder: true } ? Visibility.Visible : Visibility.Collapsed;

    public string TabletAppLabel => _s?.VersionMismatch is { } m ? $"Tablet app {m.HostVersion}" : "";

    internal string? TabletAppUrl => _s?.VersionMismatch?.TabletAppUrl;

    internal string? TabletVersion => _s?.VersionMismatch?.TabletVersion;
    public bool HasError => !string.IsNullOrEmpty(_s?.LastError);

    // ---- Diagnostics -------------------------------------------------------

    public string DiagnosticsText
    {
        get
        {
            if (_s?.Session is not { } s) return "No active session.";
            var m = s.Metrics;
            return string.Join(Environment.NewLine, new[]
            {
                $"Tablet: {s.DeviceName} ({s.DeviceModel}) at {s.RemoteAddress}, ID {s.FingerprintShort}",
                $"Phase: {s.Phase}, connected since {DateTimeOffset.FromUnixTimeSeconds((long)s.ConnectedSince).ToLocalTime():T}",
                $"Display: {s.DisplayId} {s.Width}×{s.Height}{(s.VirtualDisplay ? " (virtual)" : "")}",
                $"Codec: {Codec(s.Codec)} via {s.Encoder}",
                $"Capture: {m.CaptureFps:0.0} fps · Encode: {m.EncodeFps:0.0} fps, {m.EncodeMs:0.0} ms per frame",
                $"Bitrate: sending {m.SendMbps:0.00} Mbit/s · target {m.TargetMbps:0.0} Mbit/s · estimate {(m.EstimateMbps is { } e ? $"{e:0.0} Mbit/s" : "–")}",
                $"Network: RTT {(m.RttMs is { } r ? $"{r:0} ms" : "–")} · loss {(m.LossPercent is { } l ? $"{l:0.0} %" : "–")} · NACKs {m.Nacks} · PLIs {m.Plis}",
                $"Tablet: {(m.ClientFps is { } f ? $"{f:0} fps rendered" : "–")} · decode {(m.ClientDecodeMs is { } d ? $"{d:0.0} ms" : "–")}",
                $"Frames dropped: {m.FramesDropped} · keyframes sent: {m.Keyframes}",
            });
        }
    }

    public string EncodersText => _s is null || _s.Encoders.Length == 0
        ? "No working hardware encoder was found. Update your graphics driver."
        : string.Join(Environment.NewLine, _s.Encoders.Select(e => $"{Codec(e.Codec)}: {e.Name} (up to {e.MaxWidth}×{e.MaxHeight})"));

    public string NetworkText => _s is null ? "" :
        _s.Listening
            ? $"Listening on port {_s.Port} · {(_s.Settings.Discoverable ? "discoverable on the local network" : "not discoverable")} · addresses {string.Join(", ", _s.Addresses)}"
            : "Not listening: " + (_s.LastError ?? "unknown error");

    public string DataDirectory => _s?.DataDir ?? "";

    // ---- Display / driver --------------------------------------------------

    public string DriverTitle => _s?.Driver.State switch
    {
        DriverState.Ready => "Virtual Display Driver is ready",
        DriverState.NotInstalled => "Virtual Display Driver is not installed",
        DriverState.Unavailable => "Virtual Display Driver is not running",
        DriverState.NotApplicable => "Mirroring an existing monitor",
        _ => "Checking driver…",
    };

    public string DriverDetail => _s?.Driver.Detail ?? "";
    public bool DriverNeedsInstall => _s?.Driver.State is DriverState.NotInstalled or DriverState.Unavailable;
    public Visibility DriverInstallVisibility => DriverNeedsInstall ? Visibility.Visible : Visibility.Collapsed;

    // ---- Settings (applied immediately) -------------------------------------

    private void Push(Func<SettingsView, SettingsView> change)
    {
        if (_updating || _s is null) return;
        var next = change(_s.Settings);
        _host.Host.UpdateSettings(next);
    }

    public string HostNameSetting
    {
        get => _s?.Settings.HostName ?? "";
        set => Push(s => s with { HostName = string.IsNullOrWhiteSpace(value) ? null : value.Trim() });
    }

    public double Port
    {
        get => _s?.Settings.ControlPort ?? 47800;
        set
        {
            if (double.IsNaN(value) || value < 1 || value > 65535) return;
            Push(s => s with { ControlPort = (ushort)value });
        }
    }

    public bool Discoverable
    {
        get => _s?.Settings.Discoverable ?? true;
        set => Push(s => s with { Discoverable = value });
    }

    private static readonly string[] CodecsData = ["Automatic (best available)", "H.264", "HEVC", "AV1"];

    public int CodecIndex
    {
        get => _s?.Settings.PreferredCodec is { } c ? (int)c + 1 : 0;
        set
        {
            if (value < 0) return;
            Push(s => s with { PreferredCodec = value == 0 ? null : (VideoCodec)(value - 1) });
        }
    }

    /// <summary>Index of the entry of <paramref name="values"/> closest to <paramref name="value"/>.</summary>
    private static int Nearest(uint[] values, uint value) =>
        values.Select((v, i) => (Distance: Math.Abs((long)v - value), i)).MinBy(x => x.Distance).i;

    private static readonly uint[] FpsLimitData = [30, 60, 90, 120, 144];
    private static readonly string[] FpsLimitLabels = ["No limit", .. FpsLimitData.Select(f => $"{f} fps")];

    public string[] FpsLimitOptions => FpsLimitLabels;

    /// <summary>0: no limit, then <see cref="FpsLimitData"/>.</summary>
    public int FpsLimitIndex
    {
        get => _s?.Settings.FpsLimit is { } l ? Nearest(FpsLimitData, l) + 1 : 0;
        set
        {
            if (_updating || value < 0 || value == FpsLimitIndex) return;
            Push(s => s with { FpsLimit = value == 0 ? null : FpsLimitData[value - 1] });
        }
    }

    public bool LimitBitrate
    {
        get => _s?.Settings.MaxBitrateMbps > 0;
        set
        {
            if (_updating || value == LimitBitrate) return;
            Push(s => s with { MaxBitrateMbps = value ? 40u : 0u });
        }
    }

    public Visibility BitrateLimitVisibility => LimitBitrate ? Visibility.Visible : Visibility.Collapsed;

    public double MaxBitrate
    {
        get => _s?.Settings.MaxBitrateMbps ?? 0;
        set
        {
            if (double.IsNaN(value) || value < 1 || (uint)value == _s?.Settings.MaxBitrateMbps) return;
            Push(s => s with { MaxBitrateMbps = (uint)value });
        }
    }

    private static readonly uint[] LingerData = [0, 10, 30, 60, 300];
    private static readonly string[] LingerLabels = ["Remove it right away", "10 seconds", "30 seconds", "1 minute", "5 minutes"];

    public string[] LingerOptions => LingerLabels;

    public int LingerIndex
    {
        get => Nearest(LingerData, _s?.Settings.DisplayLingerSecs ?? 30);
        set
        {
            if (_updating || value < 0 || value == LingerIndex) return;
            Push(s => s with { DisplayLingerSecs = LingerData[value] });
        }
    }

    public bool StartWithWindows
    {
        get => _s?.Settings.StartWithWindows ?? false;
        set
        {
            if (_updating) return;
            StartupService.Apply(value, StartMinimized);
            Push(s => s with { StartWithWindows = value });
        }
    }

    public bool StartMinimized
    {
        get => _s?.Settings.StartMinimized ?? true;
        set
        {
            if (_updating) return;
            StartupService.Apply(StartWithWindows, value);
            Push(s => s with { StartMinimized = value });
        }
    }

    public bool CloseToTray
    {
        get => _s?.Settings.CloseToTray ?? true;
        set => Push(s => s with { CloseToTray = value });
    }

    private static readonly string[] ThemesData = ["Use Windows setting", "Light", "Dark"];

    public int ThemeIndex
    {
        get => (int)(_s?.Settings.Theme ?? AppTheme.System);
        set
        {
            if (value < 0) return;
            Push(s => s with { Theme = (AppTheme)value });
        }
    }

    public ElementTheme ElementTheme => (_s?.Settings.Theme ?? AppTheme.System) switch
    {
        AppTheme.Light => ElementTheme.Light,
        AppTheme.Dark => ElementTheme.Dark,
        _ => ElementTheme.Default,
    };

    public bool CheckForUpdates
    {
        get => _s?.Settings.CheckForUpdates ?? true;
        set => Push(s => s with { CheckForUpdates = value });
    }

    public bool VerboseLogging
    {
        get => _s?.Settings.VerboseLogging ?? false;
        set => Push(s => s with { VerboseLogging = value });
    }

    // ---- Encoder GPU --------------------------------------------------------

    private string GpuName(GpuKind kind) =>
        _s?.EncoderGpu.Gpus.FirstOrDefault(g => g.Kind == kind)?.Name ?? (kind == GpuKind.Integrated ? "integrated GPU" : "dedicated GPU");

    private string[] _encoderGpuOptions = [];

    /// <summary>The same array instance while the names don't change, so the
    /// combo box keeps its selection across snapshots.</summary>
    public string[] EncoderGpuOptions
    {
        get
        {
            string[] options =
            [
                "Automatic (recommended)",
                $"Integrated graphics ({GpuName(GpuKind.Integrated)}): uses less power",
                $"Dedicated graphics ({GpuName(GpuKind.Dedicated)}): more headroom, uses more power",
            ];
            if (!options.SequenceEqual(_encoderGpuOptions)) _encoderGpuOptions = options;
            return _encoderGpuOptions;
        }
    }

    public int EncoderGpuIndex
    {
        // 0: automatic, then the GPU kinds.
        get => _s?.Settings.EncoderGpu is { } k ? (int)k + 1 : 0;
        set
        {
            if (value < 0 || value == EncoderGpuIndex) return;
            Push(s => s with { EncoderGpu = value == 0 ? null : (GpuKind)(value - 1) });
        }
    }

    public Visibility EncoderGpuChoiceVisibility =>
        _s?.EncoderGpu.ChoiceAvailable == true ? Visibility.Visible : Visibility.Collapsed;

    public bool RestartRequired => _s?.EncoderGpu.RestartRequired == true;

    public bool HasEncoderGpuProblem => !string.IsNullOrEmpty(_s?.EncoderGpu.Problem);

    public string EncoderGpuProblem => _s?.EncoderGpu.Problem is { } p && _s.Settings.EncoderGpu is { } kind
        ? $"The {GpuName(kind)} cannot encode, so the other GPU is used. {p}"
        : "";

    public string ActiveEncoderText
    {
        get
        {
            var active = _s?.EncoderGpu.Gpus.FirstOrDefault(g => g.Active);
            if (active is null) return "No working hardware encoder.";
            return _s?.Settings.EncoderGpu is null && _s?.EncoderGpu.ChoiceAvailable == true
                ? $"Encoding on {active.Name}, the GPU Windows runs SecondScrn on. Change it here or in Windows Settings → System → Display → Graphics."
                : $"Encoding on {active.Name}.";
        }
    }

    // ---- Encoder benchmark --------------------------------------------------

    private static readonly int[] BenchFpsData = [60, 90, 120, 144];
    private (uint W, uint H)? _lastStreamSize;

    /// Sizes to benchmark: the last tablet stream (this run) first.
    private (uint W, uint H)[] BenchSizes
    {
        get
        {
            if (_s?.Session is { Width: > 0 } s) _lastStreamSize = (s.Width, s.Height);
            (uint, uint)[] common = [(1920, 1200), (2560, 1600), (2880, 1800), (3840, 2160)];
            return _lastStreamSize is { } last ? [last, .. common.Where(c => c != last)] : common;
        }
    }

    private string[] _benchSizeOptions = [];

    public string[] BenchSizeOptions
    {
        get
        {
            var options = BenchSizes
                .Select((z, i) => i == 0 && _lastStreamSize is not null ? $"{z.W} × {z.H} (your tablet)" : $"{z.W} × {z.H}")
                .ToArray();
            if (!options.SequenceEqual(_benchSizeOptions)) _benchSizeOptions = options;
            return _benchSizeOptions;
        }
    }

    private int _benchSizeIndex;

    public int BenchSizeIndex
    {
        get => _benchSizeIndex;
        set
        {
            // -1 while the combo box swaps its items.
            if (value >= 0) _benchSizeIndex = value;
        }
    }

    public int[] BenchFpsOptions => BenchFpsData;

    public int BenchFpsIndex { get; set; } = 2;

    public bool BenchmarkRunning => _s?.Benchmark.Running == true;

    public bool CanRunBenchmark => _s is not null && !BenchmarkRunning;

    public string BenchmarkText
    {
        get
        {
            var b = _s?.Benchmark;
            if (b is null || (b.Width == 0 && !b.Running)) return "";
            if (b.Running) return $"Measuring {b.Width} × {b.Height} at {b.Fps} fps… (about 10 seconds)";
            if (b.Error is { } e) return $"The benchmark failed: {e}";
            var lines = b.Results.Select(r => r.Error is { } err
                ? $"{r.Gpu}: could not be measured ({err})"
                : $"{(r.PacedFps >= b.Fps * 0.95 ? "✓" : "⚠")} {r.Gpu}: {r.PacedFps:0} of {b.Fps} fps, {r.LatencyMs:0.0} ms per frame, up to {r.MaxFps:0} fps");
            return $"{b.Width} × {b.Height} at {b.Fps} fps:\n" + string.Join("\n", lines);
        }
    }

    internal void RunBenchmark()
    {
        var sizes = BenchSizes;
        var (w, h) = sizes[Math.Clamp(BenchSizeIndex, 0, sizes.Length - 1)];
        _host.Host.RunBenchmark(w, h, (uint)BenchFpsData[Math.Clamp(BenchFpsIndex, 0, BenchFpsData.Length - 1)]);
    }

    // ---- Commands ----------------------------------------------------------

    internal void Disconnect() => _host.Host.Disconnect();

    internal void Revoke(string fingerprint) => _host.Host.RevokeDevice(fingerprint);

    internal void SetDevicePreferences(DeviceItem d) =>
        _host.Host.SetDevicePreferences(d.Fingerprint, d.PresetIndex == 0 ? null : (Preset)(d.PresetIndex - 1), d.AutoReconnect);

    internal string ExportDiagnostics() => _host.Host.ExportDiagnostics();

    internal IReadOnlyList<string> RecentLogs(uint max) => _host.Host.RecentLogs(max);

    internal void Refresh() => _host.Host.Refresh();
}
