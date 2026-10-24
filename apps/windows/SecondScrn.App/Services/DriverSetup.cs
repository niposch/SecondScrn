using System.ComponentModel;
using System.Diagnostics;

namespace SecondScrn.Services;

/// <summary>
/// Runs the elevated driver setup script. This is the only operation that
/// requests administrator rights, and only when the user asks for it.
/// </summary>
internal static class DriverSetup
{
    public static string ScriptPath => Path.Combine(AppContext.BaseDirectory, "Driver", "setup-vdd.ps1");

    /// <returns>null on success, otherwise a user-facing error.</returns>
    public static async Task<string?> InstallAsync()
    {
        if (!File.Exists(ScriptPath)) return $"Setup script not found: {ScriptPath}";
        var psi = new ProcessStartInfo
        {
            FileName = "powershell.exe",
            Arguments = $"-NoProfile -ExecutionPolicy Bypass -File \"{ScriptPath}\"",
            UseShellExecute = true,
            Verb = "runas",
        };
        try
        {
            using var p = Process.Start(psi);
            if (p is null) return "Could not start the driver setup.";
            await p.WaitForExitAsync();
            return p.ExitCode == 0
                ? null
                : $"Driver setup failed (exit code {p.ExitCode}). See %TEMP%\\secondscrn-vdd-install.log.";
        }
        catch (Win32Exception e) when (e.NativeErrorCode == 1223)
        {
            return "Driver setup was cancelled.";
        }
    }
}
