<#
.SYNOPSIS
  Installs the VirtualDrivers Virtual Display Driver (MttVDD) for SecondScrn.

.DESCRIPTION
  This is the only privileged step SecondScrn needs. It is deliberately kept
  out of the main application: the app itself never runs elevated.

  - Uses the upstream, SignPath-signed driver package (not modified/repackaged).
  - Creates C:\VirtualDisplayDriver\vdd_settings.xml with monitor count 0 so no
    virtual monitor appears until a tablet connects. An existing settings file
    is never overwritten.
  - Grants the local Users group modify rights on the settings file so the
    (non-elevated) SecondScrn host can add the tablet's resolution. The driver's
    own control pipe is already open to all local users, so this does not
    widen local access in a meaningful way.

.PARAMETER DriverDir
  Folder containing MttVDD.inf / MttVDD.dll / mttvdd.cat (x64).

.PARAMETER DevconPath
  Path to devcon.exe (ships with VDD Control).
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)] [string] $DriverDir,
    [Parameter(Mandatory)] [string] $DevconPath,
    [string] $ConfigDir = 'C:\VirtualDisplayDriver',
    [string] $LogPath = "$env:TEMP\secondscrn-vdd-install.log"
)

$ErrorActionPreference = 'Stop'
Start-Transcript -Path $LogPath -Force | Out-Null
try {
    $principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'install-vdd.ps1 must run elevated.'
    }

    $inf = Join-Path $DriverDir 'MttVDD.inf'
    if (-not (Test-Path $inf)) { throw "MttVDD.inf not found in $DriverDir" }

    $sig = Get-AuthenticodeSignature (Join-Path $DriverDir 'mttvdd.cat')
    if ($sig.Status -ne 'Valid') { throw "Driver catalog signature is not valid: $($sig.Status)" }
    Write-Host "Catalog signed by: $($sig.SignerCertificate.Subject)"

    $settings = Join-Path $ConfigDir 'vdd_settings.xml'
    if (-not (Test-Path $settings)) {
        New-Item -ItemType Directory -Force -Path $ConfigDir | Out-Null
        [xml] $xml = Get-Content (Join-Path $DriverDir 'vdd_settings.xml') -Raw
        $xml.vdd_settings.monitors.count = '0'
        $xml.Save($settings)
        Write-Host "Created $settings (monitor count 0)"
    } else {
        Write-Host "Keeping existing $settings"
    }

    $acl = Get-Acl $settings
    $users = New-Object Security.Principal.SecurityIdentifier('S-1-5-32-545')
    $rule = New-Object Security.AccessControl.FileSystemAccessRule($users, 'Modify', 'Allow')
    $acl.AddAccessRule($rule)
    Set-Acl $settings $acl

    $existing = Get-PnpDevice -ErrorAction SilentlyContinue |
        Where-Object { $_.HardwareID -contains 'Root\MttVDD' }
    if ($existing) {
        Write-Host "MttVDD device already present ($($existing.InstanceId)); updating driver only."
        & $DevconPath update $inf 'Root\MttVDD'
    } else {
        & $DevconPath install $inf 'Root\MttVDD'
    }
    if ($LASTEXITCODE -ne 0 -and $LASTEXITCODE -ne 1) { throw "devcon failed with exit code $LASTEXITCODE" }
    Write-Host 'Virtual Display Driver installed.'
}
finally {
    Stop-Transcript | Out-Null
}
