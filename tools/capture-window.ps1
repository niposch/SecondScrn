<#
.SYNOPSIS
  Saves a PNG of one top-level window of a process (PrintWindow), e.g. to
  check the SecondScrn UI in automated runs. Captures only that window, not
  the rest of the desktop.
#>
param(
    [Parameter(Mandatory)] [string] $ProcessName,
    [Parameter(Mandatory)] [string] $TitleLike,
    [Parameter(Mandatory)] [string] $OutFile,
    [switch] $Exact
)
Add-Type -ReferencedAssemblies System.Drawing -TypeDefinition @'
using System;
using System.Drawing;
using System.Runtime.InteropServices;
using System.Text;
public static class Cap {
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc p, IntPtr l);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
    [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr v);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
    public static IntPtr Find(uint pid, string title, bool exact) {
        IntPtr found = IntPtr.Zero;
        EnumWindows((h, l) => {
            uint p; GetWindowThreadProcessId(h, out p);
            if (p != pid || !IsWindowVisible(h)) return true;
            var sb = new StringBuilder(256); GetWindowText(h, sb, 256);
            if (exact ? sb.ToString() == title : sb.ToString().Contains(title)) { found = h; return false; }
            return true;
        }, IntPtr.Zero);
        return found;
    }
    public static void Save(IntPtr h, string path) {
        RECT r; GetWindowRect(h, out r);
        using (var bmp = new Bitmap(r.R - r.L, r.B - r.T))
        using (var g = Graphics.FromImage(bmp)) {
            var hdc = g.GetHdc(); PrintWindow(h, hdc, 2); g.ReleaseHdc(hdc);
            bmp.Save(path, System.Drawing.Imaging.ImageFormat.Png);
        }
    }
}
'@
[Cap]::SetProcessDpiAwarenessContext([IntPtr]-4) | Out-Null
$p = Get-Process $ProcessName -ErrorAction Stop | Select-Object -First 1
$h = [Cap]::Find([uint32]$p.Id, $TitleLike, $Exact.IsPresent)
if ($h -eq [IntPtr]::Zero) { throw "window '$TitleLike' of $ProcessName not found" }
[Cap]::Save($h, $OutFile)
Write-Output "saved $OutFile"
