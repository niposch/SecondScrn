using Microsoft.Win32;

namespace SecondScrn.Services;

/// <summary>"Start with Windows" through the per-user Run key (no elevation).</summary>
internal static class StartupService
{
    private const string RunKey = @"Software\Microsoft\Windows\CurrentVersion\Run";
    private const string ValueName = "SecondScrn";

    public static void Apply(bool enabled, bool minimized)
    {
        using var key = Registry.CurrentUser.CreateSubKey(RunKey, writable: true);
        if (enabled)
        {
            // Velopack keeps the current version under ...\current\, so the
            // path stays valid across updates.
            var exe = Environment.ProcessPath ?? Path.Combine(AppContext.BaseDirectory, "SecondScrn.exe");
            key.SetValue(ValueName, minimized ? $"\"{exe}\" --minimized" : $"\"{exe}\"");
        }
        else if (key.GetValue(ValueName) is not null)
        {
            key.DeleteValue(ValueName);
        }
    }
}
